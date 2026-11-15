//! Dry-run, mutation-mode, dependency-ordering, and plan-rendering tests for the reconcile action.

use std::{
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
    reconcile::{DesiredStateTracker, ReconcileAction, ScanLimits, scan_directory},
    status::{LoggerLastResult, SpawnDetail, StatusSnapshot},
    supervisor::StateMachine,
};

use super::{
    ActionError, DesiredPlan, DirectoryAction, ServiceFailure, ServiceFailureKind, StartBlocker,
    desired_plan, execute_with_endpoint, write_plan,
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

/// A start that times out behind a failed logger must say so, and prefer the
/// logger over the service because the service is not attempted without it.
#[test]
fn lifecycle_timeout_names_what_blocks_the_start() {
    let detail = SpawnDetail {
        stage: SpawnStage::Execute,
        failure: SpawnFailure::OperatingSystem,
        os_error: Some(2),
    };
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    assert_eq!(StartBlocker::from_status(&status), None);
    assert_eq!(
        ServiceFailureKind::LifecycleTimeout(None).to_string(),
        "lifecycle deadline exceeded"
    );

    status.last_spawn_failure = Some(detail);
    assert_eq!(
        StartBlocker::from_status(&status),
        Some(StartBlocker::Service(detail))
    );
    status.logger_last_result = Some(LoggerLastResult::SpawnFailed(detail));
    let blocker = StartBlocker::from_status(&status);
    assert_eq!(
        blocker,
        Some(StartBlocker::Logger(LoggerLastResult::SpawnFailed(detail)))
    );
    let diagnostic = ServiceFailureKind::LifecycleTimeout(blocker).to_string();
    assert!(diagnostic.starts_with("lifecycle deadline exceeded: logger execution failed: "));
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
    let mut tracker = DesiredStateTracker::default();
    let summary = write_plan(&mut tracker, scan, &mut stdout, &mut stderr)?;

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
    let mut tracker = DesiredStateTracker::default();
    let summary = write_plan(&mut tracker, scan, &mut stdout, &mut Vec::new())?;

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

#[test]
fn repeated_plans_confirm_deletion_without_duplicate_stop() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.yml");
    fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
    let mut tracker = DesiredStateTracker::default();

    let mut stdout = Vec::new();
    write_plan(
        &mut tracker,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut stdout,
        &mut Vec::new(),
    )?;
    assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");

    fs::remove_file(path)?;
    let mut first_missing = Vec::new();
    write_plan(
        &mut tracker,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut first_missing,
        &mut Vec::new(),
    )?;
    assert_eq!(String::from_utf8(first_missing)?, "KEEP\tapi\n");

    let mut confirmed = Vec::new();
    write_plan(
        &mut tracker,
        scan_directory(directory.path(), ScanLimits::default())?,
        &mut confirmed,
        &mut Vec::new(),
    )?;
    assert_eq!(String::from_utf8(confirmed)?, "STOP\tapi\n");

    let mut repeated = Vec::new();
    write_plan(
        &mut tracker,
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
