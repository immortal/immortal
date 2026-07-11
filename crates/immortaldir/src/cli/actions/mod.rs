//! Coordination of directory scans and operational reconciliation.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
    time::Duration,
};

use immortal_core::exit::ExitClass;
use immortal_core::reconcile::{
    DesiredStateTracker, ReconcileAction, ScanError, ScanLimits, ScanResult,
    canonical_definitions_directory, scan_directory,
};
use immortal_core::watch::{DEFAULT_DEBOUNCE, ReconcileTriggers, WatchError};

use crate::cli::dispatch::Action;

/// Failure while executing a reconciliation action.
#[derive(Debug)]
pub enum ActionError {
    /// Definitions directory could not be scanned.
    Scan(ScanError),
    /// Plan or diagnostic output failed.
    Output(io::Error),
    /// Native watcher initialization failed.
    Watch(WatchError),
    /// Supervisor mutation/watch loop has not crossed its implementation gate.
    OperationalUnavailable,
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Scan(error) => Display::fmt(error, formatter),
            Self::Output(error) => {
                write!(formatter, "unable to write reconciliation output: {error}")
            }
            Self::Watch(error) => Display::fmt(error, formatter),
            Self::OperationalUnavailable => formatter.write_str(
                "supervisor mutations are not enabled yet; use --dry-run to inspect definitions",
            ),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Scan(error) => Some(error),
            Self::Output(error) => Some(error),
            Self::Watch(error) => Some(error),
            Self::OperationalUnavailable => None,
        }
    }
}

impl From<ScanError> for ActionError {
    fn from(error: ScanError) -> Self {
        Self::Scan(error)
    }
}

impl From<io::Error> for ActionError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

impl From<WatchError> for ActionError {
    fn from(error: WatchError) -> Self {
        Self::Watch(error)
    }
}

impl ActionError {
    /// Stable process exit classification for this failure.
    #[must_use]
    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::Scan(_) => ExitClass::Configuration,
            Self::Output(_) => ExitClass::IoError,
            Self::Watch(_) | Self::OperationalUnavailable => ExitClass::Unavailable,
        }
    }
}

/// Execute one typed reconciliation action.
///
/// # Errors
///
/// Returns an error when scanning or output fails, or when mutation/watch mode
/// is requested before its process-control contracts are implemented.
pub async fn execute(action: &Action) -> Result<(), ActionError> {
    if !action.dry_run {
        return Err(ActionError::OperationalUnavailable);
    }
    let directory = canonical_definitions_directory(&action.directory)?;
    let mut tracker = DesiredStateTracker::default();
    if action.once {
        return scan_and_print(&directory, &mut tracker);
    }

    let mut triggers = ReconcileTriggers::with_intervals(
        &directory,
        DEFAULT_DEBOUNCE,
        Duration::from_secs(action.scan_interval_seconds),
    )?;
    loop {
        let trigger = triggers.next().await;
        for error in trigger.watcher_errors {
            writeln!(io::stderr().lock(), "watcher: {error}")?;
        }
        scan_and_print(&directory, &mut tracker)?;
    }
}

fn scan_and_print(
    directory: &std::path::Path,
    tracker: &mut DesiredStateTracker,
) -> Result<(), ActionError> {
    let scan = scan_directory(directory, ScanLimits::default())?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    write_plan(tracker, scan, &mut stdout, &mut stderr)?;
    Ok(())
}

fn write_plan(
    tracker: &mut DesiredStateTracker,
    scan: ScanResult,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    let actions = tracker.apply(&scan);
    for (name, action) in actions {
        let verb = match action {
            ReconcileAction::Start => "START",
            ReconcileAction::Restart => "RESTART",
            ReconcileAction::Stop => "STOP",
            ReconcileAction::Keep => "KEEP",
        };
        writeln!(stdout, "{verb}\t{name}")?;
    }
    for (name, definition) in scan.definitions {
        for warning in definition.parsed.warnings {
            writeln!(stderr, "{}: warning: {}", name, warning.message)?;
        }
    }
    for problem in scan.problems {
        writeln!(stderr, "{}: {:?}", problem.path.display(), problem.kind)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use immortal_core::reconcile::{DesiredStateTracker, ScanLimits, scan_directory};

    use super::{ActionError, execute, write_plan};
    use crate::cli::dispatch::Action;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortaldir-action-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
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

    fn action(directory: &Path, dry_run: bool) -> Action {
        Action {
            directory: directory.to_owned(),
            runtime_directory: PathBuf::from("/unused"),
            scan_interval_seconds: 30,
            once: true,
            dry_run,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_shot_dry_run_scans_successfully() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.path().join("api.yml"), "cmd: /bin/true\n")?;
        execute(&action(directory.path(), true)).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mutation_mode_remains_explicitly_unavailable() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        assert!(matches!(
            execute(&action(directory.path(), false)).await,
            Err(ActionError::OperationalUnavailable)
        ));
        Ok(())
    }

    #[test]
    fn plan_output_is_deterministic_and_keeps_diagnostics_separate() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(directory.path().join("api.yml"), "cmd: /bin/true\n")?;
        fs::write(directory.path().join("broken.yml"), "cmd: []\n")?;
        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut tracker = DesiredStateTracker::default();
        write_plan(&mut tracker, scan, &mut stdout, &mut stderr)?;

        assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");
        let diagnostics = String::from_utf8(stderr)?;
        assert!(diagnostics.contains("broken.yml"));
        assert!(diagnostics.contains("api: warning:"));
        Ok(())
    }

    #[test]
    fn repeated_plans_confirm_deletion_without_duplicate_stop() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "cmd: /bin/true\n")?;
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
}
