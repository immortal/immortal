//! Dry-run, mutation-mode, dependency-ordering, and plan-rendering tests for the reconcile action.

use std::{
    collections::BTreeMap,
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use immortal_core::{
    config::ServiceConfig,
    exit::ExitClass,
    process::{SpawnFailure, SpawnStage},
    reconcile::{DefinitionSnapshots, ReconcileAction, ScanLimits, scan_directory},
    status::{LastResult, LoggerLastResult, ServiceState, SpawnDetail, StatusSnapshot},
    supervisor::StateMachine,
};

use super::{
    ActionError, DesiredPlan, DirectoryAction, OperationalState, PlanPreview, ServiceFailure,
    ServiceFailureKind, StartBlocker, desired_plan, execute_with_endpoint, write_plan,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new("/var/tmp").join(format!(
            "immortaldir-action-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        // Definitions directories are a trust boundary, so fixtures must not
        // inherit a group-writable umask.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        Ok(Self(fs::canonicalize(path)?))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn action(directory: &Path, dry_run: bool) -> DirectoryAction {
    DirectoryAction {
        directory: directory.to_owned(),
        runtime_directory: PathBuf::from("/unused"),
        scan_interval_seconds: 30,
        supervisor_binary: PathBuf::from("immortal"),
        launch_concurrency: immortal_core::reconcile::LaunchConcurrency::default(),
        once: true,
        dry_run,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn one_shot_dry_run_scans_successfully() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;
    execute_with_endpoint(&action(directory.path(), true), None).await?;
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn mutation_mode_requires_a_pre_runtime_broker() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    assert!(matches!(
        execute_with_endpoint(&action(directory.path(), false), None).await,
        Err(ActionError::MissingBroker)
    ));
    Ok(())
}

const EXECUTE_FAILED: SpawnDetail = SpawnDetail {
    stage: SpawnStage::Execute,
    failure: SpawnFailure::OperatingSystem,
    os_error: Some(2),
};

/// A status in `state` whose last start failed, as a supervisor publishes it.
fn status_after_failed_start(state: ServiceState) -> StatusSnapshot {
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.state = state;
    status.last_result = Some(LastResult::SpawnFailed);
    status.last_spawn_failure = Some(EXECUTE_FAILED);
    status
}

/// A start that times out behind a failed logger must say so, and prefer the
/// logger over the service because the service is not attempted without it.
#[test]
fn lifecycle_timeout_names_what_blocks_the_start() {
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    assert_eq!(StartBlocker::observe(None, Some(&status)), None);
    assert_eq!(
        ServiceFailureKind::LifecycleTimeout(None).to_string(),
        "lifecycle deadline exceeded"
    );

    status = status_after_failed_start(ServiceState::Backoff);
    assert_eq!(
        StartBlocker::observe(None, Some(&status)),
        Some(StartBlocker::Service(EXECUTE_FAILED))
    );
    status.logger_last_result = Some(LoggerLastResult::SpawnFailed(EXECUTE_FAILED));
    let blocker = StartBlocker::observe(None, Some(&status));
    assert_eq!(
        blocker,
        Some(StartBlocker::Logger(LoggerLastResult::SpawnFailed(
            EXECUTE_FAILED
        )))
    );
    let diagnostic = ServiceFailureKind::LifecycleTimeout(blocker).to_string();
    assert!(diagnostic.starts_with("lifecycle deadline exceeded: logger execution failed: "));
}

/// Regression: a start that failed, was repaired, and then ran without
/// declaring readiness timed out blaming the earlier failed start, which
/// status keeps as history.
#[test]
fn lifecycle_timeout_does_not_blame_a_start_that_later_succeeded() {
    let failed = status_after_failed_start(ServiceState::Backoff);
    let retrying = status_after_failed_start(ServiceState::Starting);
    let mut running = status_after_failed_start(ServiceState::Running);
    // A logger which fails after the service runs does not hold it back.
    running.logger_last_result = Some(LoggerLastResult::Exited(1));

    let blocker = StartBlocker::observe(None, Some(&failed));
    let blocker = StartBlocker::observe(blocker, Some(&retrying));
    assert_eq!(
        blocker,
        Some(StartBlocker::Service(EXECUTE_FAILED)),
        "a retry in progress has no outcome yet, so the known cause stands"
    );

    let blocker = StartBlocker::observe(blocker, Some(&running));
    assert_eq!(blocker, Some(StartBlocker::Readiness));
    assert_eq!(
        ServiceFailureKind::LifecycleTimeout(blocker).to_string(),
        "lifecycle deadline exceeded: service has not declared readiness"
    );
}

/// Regression: the cause survived polls where the supervisor was gone.
#[test]
fn lifecycle_timeout_forgets_a_cause_when_the_supervisor_disappears() {
    let failed = status_after_failed_start(ServiceState::Failed);
    let blocker = StartBlocker::observe(None, Some(&failed));
    assert_eq!(blocker, Some(StartBlocker::Service(EXECUTE_FAILED)));

    assert_eq!(StartBlocker::observe(blocker, None), None);
}

#[test]
fn lifecycle_timeout_names_no_blocker_for_a_service_past_starting() {
    for state in [
        ServiceState::Ready,
        ServiceState::Paused,
        ServiceState::Stopping,
        ServiceState::Completed,
        ServiceState::Exited,
    ] {
        let mut status = status_after_failed_start(state);
        status.logger_last_result = Some(LoggerLastResult::Signaled(9));
        assert_eq!(
            StartBlocker::observe(Some(StartBlocker::Readiness), Some(&status)),
            None,
            "{}",
            state.name()
        );
    }
}

#[test]
fn partial_failures_use_a_bounded_diagnostic_and_partial_exit() {
    let failures = (0..17)
        .map(|sequence| {
            ServiceFailure::new(
                &format!("service-{sequence}"),
                ServiceFailureKind::Io(io::Error::other("injected")),
            )
        })
        .collect();
    let error = ActionError::Partial(failures);
    assert_eq!(error.exit_class(), ExitClass::PartialFailure);
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("17 service mutation(s) failed"));
    assert!(diagnostic.contains("1 additional failure(s) omitted"));
    assert!(!diagnostic.contains("service-16"));
}

#[test]
fn plan_output_is_deterministic_and_keeps_diagnostics_separate() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;
    fs::write(
        directory.path().join("broken.yml"),
        "version: 2\ncommand: []\n",
    )?;
    let scan = scan_directory(directory.path(), ScanLimits::default())?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut preview = PlanPreview::default();
    let summary = write_plan(&mut preview, scan, &mut stdout, &mut stderr)?;

    assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");
    let diagnostics = String::from_utf8(stderr)?;
    assert!(diagnostics.contains("broken.yml"));
    assert_eq!(summary.invalid_definitions, 1);
    assert!(summary.unresolvable.is_empty());
    Ok(())
}

/// The preview must withhold the start a real pass withholds: a service whose
/// requirement is invalid, and every service behind it, is not listed.
#[test]
fn plan_withholds_starts_behind_an_invalid_requirement() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: []\n",
    )?;
    fs::write(
        directory.path().join("worker.yml"),
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    fs::write(
        directory.path().join("report.yml"),
        "version: 2\ncommand: [/bin/true]\nrequires: [worker]\n",
    )?;
    fs::write(
        directory.path().join("web.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;
    let scan = scan_directory(directory.path(), ScanLimits::default())?;
    let mut stdout = Vec::new();
    let mut preview = PlanPreview::default();
    let summary = write_plan(&mut preview, scan, &mut stdout, &mut Vec::new())?;

    assert_eq!(String::from_utf8(stdout)?, "START\tweb\n");
    assert_eq!(summary.invalid_definitions, 1);
    let blocked: Vec<String> = summary
        .unresolvable
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(blocked.len(), 2);
    assert!(blocked.iter().any(|failure| failure.contains("`report`")));
    assert!(
        blocked
            .iter()
            .any(|failure| failure.contains("`worker`") && failure.contains("`api`"))
    );
    Ok(())
}

/// A one-shot dry run gates a migration, so an invalid definition must fail
/// it with the configuration status even though the valid plan was printed.
#[tokio::test(flavor = "current_thread")]
async fn one_shot_dry_run_fails_on_an_invalid_definition() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;
    fs::write(
        directory.path().join("broken.yml"),
        "version: 2\ncommand: [\n",
    )?;
    let result = execute_with_endpoint(&action(directory.path(), true), None).await;
    let Err(error) = result else {
        return Err("an invalid definition did not fail the dry run".into());
    };
    assert!(matches!(error, ActionError::InvalidDefinitions(1)));
    assert_eq!(error.exit_class(), ExitClass::Configuration);
    Ok(())
}

/// A service that can never start fails the one-shot dry run exactly as it
/// fails the real pass, and takes precedence over the invalid definition.
#[tokio::test(flavor = "current_thread")]
async fn one_shot_dry_run_fails_on_an_unresolvable_requirement() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: []\n",
    )?;
    fs::write(
        directory.path().join("worker.yml"),
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    let result = execute_with_endpoint(&action(directory.path(), true), None).await;
    let Err(error) = result else {
        return Err("an unresolvable requirement did not fail the dry run".into());
    };
    assert!(matches!(&error, ActionError::Partial(failures) if failures.len() == 1));
    assert_eq!(error.exit_class(), ExitClass::PartialFailure);
    Ok(())
}

/// Print the plan for the directory's current contents.
fn plan(directory: &TestDirectory, preview: &mut PlanPreview) -> Result<String, Box<dyn Error>> {
    let mut stdout = Vec::new();
    write_plan(
        preview,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut stdout,
        &mut Vec::new(),
    )?;
    Ok(String::from_utf8(stdout)?)
}

/// Regression: a continuous preview forgot a withheld start, so repairing the
/// requirement listed only the requirement although a real pass starts both.
#[test]
fn successive_plans_release_a_withheld_start_once_its_requirement_is_valid()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let api = directory.path().join("api.yml");
    fs::write(&api, "version: 2\ncommand: []\n")?;
    fs::write(
        directory.path().join("worker.yml"),
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    fs::write(
        directory.path().join("report.yml"),
        "version: 2\ncommand: [/bin/true]\nrequires: [worker]\n",
    )?;
    let mut preview = PlanPreview::default();

    assert_eq!(plan(&directory, &mut preview)?, "");
    assert_eq!(
        plan(&directory, &mut preview)?,
        "",
        "an unchanged scan keeps withholding without reporting KEEP"
    );

    fs::write(&api, "version: 2\ncommand: [/bin/true]\n")?;
    assert_eq!(
        plan(&directory, &mut preview)?,
        "START\tapi\nSTART\treport\nSTART\tworker\n"
    );
    assert_eq!(
        plan(&directory, &mut preview)?,
        "KEEP\tapi\nKEEP\treport\nKEEP\tworker\n"
    );
    Ok(())
}

/// A service whose first start is withheld has never run: changing it is
/// still a start, and disabling it leaves nothing to stop.
#[test]
fn a_withheld_first_start_stays_a_start_when_changed_and_needs_no_stop()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let api = directory.path().join("api.yml");
    let worker = directory.path().join("worker.yml");
    fs::write(&api, "version: 2\ncommand: []\n")?;
    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    let mut preview = PlanPreview::default();
    assert_eq!(plan(&directory, &mut preview)?, "");

    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/false]\nrequires: [api]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "");
    fs::write(&api, "version: 2\ncommand: [/bin/true]\n")?;
    assert_eq!(
        plan(&directory, &mut preview)?,
        "START\tapi\nSTART\tworker\n"
    );

    let directory = TestDirectory::new()?;
    let worker = directory.path().join("worker.yml");
    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    let mut preview = PlanPreview::default();
    assert_eq!(plan(&directory, &mut preview)?, "");
    fs::write(
        &worker,
        "version: 2\nenabled: false\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "KEEP\tworker\n");
    Ok(())
}

/// A running service whose restart is withheld is restarted once its new
/// requirement can be met.
#[test]
fn a_withheld_restart_is_released_once_its_requirement_is_valid() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let worker = directory.path().join("worker.yml");
    fs::write(&worker, "version: 2\ncommand: [/bin/true]\n")?;
    let mut preview = PlanPreview::default();
    assert_eq!(plan(&directory, &mut preview)?, "START\tworker\n");

    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "");
    assert_eq!(plan(&directory, &mut preview)?, "");

    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;
    assert_eq!(
        plan(&directory, &mut preview)?,
        "START\tapi\nRESTART\tworker\n"
    );
    Ok(())
}

/// Regression: the preview remembered that a restart was withheld but not
/// what was running, so restoring the running definition printed a restart
/// a real pass, which compares against the applied definition, never makes.
#[test]
fn a_withheld_restart_is_dropped_when_the_running_definition_is_restored()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let worker = directory.path().join("worker.yml");
    let running = "version: 2\ncommand: [/bin/true]\n";
    fs::write(&worker, running)?;
    let mut preview = PlanPreview::default();
    assert_eq!(plan(&directory, &mut preview)?, "START\tworker\n");

    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "");

    fs::write(&worker, running)?;
    assert_eq!(plan(&directory, &mut preview)?, "KEEP\tworker\n");
    Ok(())
}

#[test]
fn a_withheld_restart_is_made_once_a_different_definition_can_be_scheduled()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let worker = directory.path().join("worker.yml");
    fs::write(&worker, "version: 2\ncommand: [/bin/true]\n")?;
    let mut preview = PlanPreview::default();
    assert_eq!(plan(&directory, &mut preview)?, "START\tworker\n");

    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "");

    fs::write(&worker, "version: 2\ncommand: [/bin/false]\n")?;
    assert_eq!(plan(&directory, &mut preview)?, "RESTART\tworker\n");
    assert_eq!(plan(&directory, &mut preview)?, "KEEP\tworker\n");
    Ok(())
}

/// Regression: editing a stopped, disabled service listed a restart, which a
/// real pass never makes because only enabled services enter a start wave.
#[test]
fn a_disabled_service_is_not_restarted_until_it_is_enabled_again() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let worker = directory.path().join("worker.yml");
    let mut preview = PlanPreview::default();
    fs::write(&worker, "version: 2\ncommand: [/bin/true]\n")?;
    assert_eq!(plan(&directory, &mut preview)?, "START\tworker\n");

    fs::write(
        &worker,
        "version: 2\nenabled: false\ncommand: [/bin/true]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "STOP\tworker\n");

    fs::write(
        &worker,
        "version: 2\nenabled: false\ncommand: [/bin/false]\n",
    )?;
    assert_eq!(plan(&directory, &mut preview)?, "KEEP\tworker\n");

    fs::write(&worker, "version: 2\ncommand: [/bin/false]\n")?;
    assert_eq!(plan(&directory, &mut preview)?, "RESTART\tworker\n");
    assert_eq!(plan(&directory, &mut preview)?, "KEEP\tworker\n");
    Ok(())
}

/// A service that was never started has nothing to stop when its definition
/// is removed.
#[test]
fn removing_a_never_started_service_prints_no_stop() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let worker = directory.path().join("worker.yml");
    fs::write(
        &worker,
        "version: 2\ncommand: [/bin/true]\nrequires: [api]\n",
    )?;
    let mut preview = PlanPreview::default();
    assert_eq!(plan(&directory, &mut preview)?, "");

    fs::remove_file(&worker)?;
    assert_eq!(plan(&directory, &mut preview)?, "");
    assert_eq!(plan(&directory, &mut preview)?, "");
    Ok(())
}

#[test]
fn repeated_plans_confirm_deletion_without_duplicate_stop() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.yml");
    fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
    let mut preview = PlanPreview::default();

    let mut stdout = Vec::new();
    write_plan(
        &mut preview,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut stdout,
        &mut Vec::new(),
    )?;
    assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");

    fs::remove_file(path)?;
    let mut first_missing = Vec::new();
    write_plan(
        &mut preview,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut first_missing,
        &mut Vec::new(),
    )?;
    assert_eq!(String::from_utf8(first_missing)?, "KEEP\tapi\n");

    let mut confirmed = Vec::new();
    write_plan(
        &mut preview,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut confirmed,
        &mut Vec::new(),
    )?;
    assert_eq!(String::from_utf8(confirmed)?, "STOP\tapi\n");

    let mut repeated = Vec::new();
    write_plan(
        &mut preview,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut repeated,
        &mut Vec::new(),
    )?;
    assert_eq!(repeated, Vec::<u8>::new());
    Ok(())
}

#[test]
fn applied_snapshot_recovers_live_action_without_in_memory_history() -> Result<(), Box<dyn Error>> {
    let enabled = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let changed = ServiceConfig::for_command(vec!["/bin/false".to_owned()])?;
    let mut disabled = enabled.clone();
    disabled.enabled = false;

    assert_eq!(desired_plan(true, &enabled, None), DesiredPlan::Adopt);
    assert_eq!(
        desired_plan(true, &enabled, Some(&enabled)),
        DesiredPlan::Keep
    );
    assert_eq!(
        desired_plan(true, &changed, Some(&enabled)),
        DesiredPlan::Apply(ReconcileAction::Restart)
    );
    assert_eq!(
        desired_plan(true, &disabled, Some(&enabled)),
        DesiredPlan::Apply(ReconcileAction::Stop)
    );
    assert_eq!(
        desired_plan(true, &enabled, Some(&disabled)),
        DesiredPlan::Apply(ReconcileAction::Restart)
    );
    assert_eq!(
        desired_plan(false, &enabled, Some(&enabled)),
        DesiredPlan::Apply(ReconcileAction::Start)
    );
    assert_eq!(
        desired_plan(false, &disabled, Some(&disabled)),
        DesiredPlan::Keep
    );
    Ok(())
}

/// Regression: a restart whose new supervisor was not Ready within the
/// lifecycle deadline was never recorded as applied, so every later scan
/// halted and relaunched that supervisor, even once it was Ready. A launch is
/// recorded as soon as it succeeds, and the next plan keeps it.
#[test]
fn a_launched_restart_is_recorded_and_kept_by_the_next_plan() -> Result<(), Box<dyn Error>> {
    let runtime = TestDirectory::new()?;
    let snapshots = DefinitionSnapshots::open(runtime.path())?;
    let previous = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let changed = ServiceConfig::for_command(vec!["/bin/false".to_owned()])?;
    let mut operational = OperationalState {
        known: BTreeMap::from([("api".to_owned(), previous)]),
        pending: BTreeMap::from([("api".to_owned(), ReconcileAction::Restart)]),
    };

    operational
        .record_launch("api", &changed, &snapshots)
        .map_err(|error| format!("{error:?}"))?;

    assert!(!operational.pending.contains_key("api"));
    assert_eq!(operational.known.get("api"), Some(&changed));
    let applied = snapshots.load_applied("api")?;
    assert_eq!(applied.as_ref(), Some(&changed));
    assert_eq!(
        desired_plan(true, &changed, applied.as_ref()),
        DesiredPlan::Keep
    );
    Ok(())
}
