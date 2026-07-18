//! Dry-run, mutation-mode, dependency-ordering, and plan-rendering tests for the reconcile action.

use std::{
    error::Error,
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use immortal_core::{
    config::ServiceConfig,
    exit::ExitClass,
    reconcile::{DesiredStateTracker, ReconcileAction, ScanLimits, scan_directory},
};

use super::{
    ActionError, DesiredPlan, DirectoryAction, ServiceFailure, ServiceFailureKind, desired_plan,
    execute_with_endpoint, write_plan,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new("/tmp").join(format!(
            "immortaldir-action-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
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
    write_plan(&mut tracker, scan, &mut stdout, &mut stderr)?;

    assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");
    let diagnostics = String::from_utf8(stderr)?;
    assert!(diagnostics.contains("broken.yml"));
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
    assert!(repeated.is_empty());
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
