//! Typed directory actions and focused reconciliation execution.
//!
//! Dispatch produces one explicit reconciliation variant. The named binary
//! routes it to the focused handler, which preserves broker-before-Tokio
//! ordering and delegates process behavior to `immortal-core`.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use immortal_core::{
    exit::ExitClass,
    reconcile::{DependencyError, LaunchConcurrency, LauncherError, ScanError},
    runtime::RuntimeRootError,
    watch::WatchError,
};

pub mod reconcile;

/// One mutually exclusive `immortaldir` operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Reconcile a definitions directory.
    Reconcile(ReconcileAction),
}

/// Typed inputs for directory reconciliation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileAction {
    /// Definitions directory.
    pub directory: PathBuf,
    /// Runtime state root reserved for operational reconciliation.
    pub runtime_directory: PathBuf,
    /// Safety reconciliation interval.
    pub scan_interval_seconds: u64,
    /// Exact `immortal` executable used for new supervisors.
    pub supervisor_binary: PathBuf,
    /// Validated maximum launches submitted within one dependency wave.
    pub launch_concurrency: LaunchConcurrency,
    /// Exit after one complete reconciliation.
    pub once: bool,
    /// Print the desired plan without mutations.
    pub dry_run: bool,
}

/// Failure while executing a reconciliation action.
#[derive(Debug)]
pub enum ActionError {
    /// Process broker creation failed before Tokio initialization.
    Broker(io::Error),
    /// Tokio could not create the bounded reconciliation runtime.
    RuntimeInitialization(io::Error),
    /// Definitions directory could not be scanned.
    Scan(ScanError),
    /// Reconciliation filesystem, snapshot, or output I/O failed.
    Io(io::Error),
    /// Native watcher initialization failed.
    Watch(WatchError),
    /// Runtime root was absent or unsafe.
    Runtime(RuntimeRootError),
    /// Desired dependencies are missing or cyclic.
    Dependency(DependencyError),
    /// Checked supervisor launcher failed.
    Launcher(LauncherError),
    /// One or more services failed without preventing independent work.
    Partial(Vec<reconcile::ServiceFailure>),
    /// Operational execution did not receive its pre-Tokio broker.
    MissingBroker,
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Broker(error) => write!(formatter, "unable to start process broker: {error}"),
            Self::RuntimeInitialization(error) => {
                write!(
                    formatter,
                    "unable to initialize reconciliation runtime: {error}"
                )
            }
            Self::Scan(error) => Display::fmt(error, formatter),
            Self::Io(error) => {
                write!(formatter, "reconciliation I/O failed: {error}")
            }
            Self::Watch(error) => Display::fmt(error, formatter),
            Self::Runtime(error) => Display::fmt(error, formatter),
            Self::Dependency(error) => Display::fmt(error, formatter),
            Self::Launcher(error) => Display::fmt(error, formatter),
            Self::Partial(failures) => {
                write!(formatter, "{} service mutation(s) failed", failures.len())?;
                for failure in failures.iter().take(MAX_DISPLAYED_FAILURES) {
                    write!(formatter, "; {failure}")?;
                }
                if failures.len() > MAX_DISPLAYED_FAILURES {
                    write!(
                        formatter,
                        "; {} additional failure(s) omitted",
                        failures.len() - MAX_DISPLAYED_FAILURES
                    )?;
                }
                Ok(())
            }
            Self::MissingBroker => formatter.write_str("operational reconciliation has no broker"),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Broker(error) | Self::RuntimeInitialization(error) | Self::Io(error) => {
                Some(error)
            }
            Self::Scan(error) => Some(error),
            Self::Watch(error) => Some(error),
            Self::Runtime(error) => Some(error),
            Self::Dependency(error) => Some(error),
            Self::Launcher(error) => Some(error),
            Self::Partial(_) | Self::MissingBroker => None,
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
        Self::Io(error)
    }
}

impl From<WatchError> for ActionError {
    fn from(error: WatchError) -> Self {
        Self::Watch(error)
    }
}

impl From<RuntimeRootError> for ActionError {
    fn from(error: RuntimeRootError) -> Self {
        Self::Runtime(error)
    }
}

impl From<DependencyError> for ActionError {
    fn from(error: DependencyError) -> Self {
        Self::Dependency(error)
    }
}

impl From<LauncherError> for ActionError {
    fn from(error: LauncherError) -> Self {
        Self::Launcher(error)
    }
}

impl ActionError {
    /// Stable process exit classification for this failure.
    #[must_use]
    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::RuntimeInitialization(_) => ExitClass::Software,
            Self::Broker(_) | Self::Launcher(_) => ExitClass::OsError,
            Self::Scan(_) | Self::Runtime(_) | Self::Dependency(_) => ExitClass::Configuration,
            Self::Io(_) => ExitClass::IoError,
            Self::Partial(_) => ExitClass::PartialFailure,
            Self::Watch(_) | Self::MissingBroker => ExitClass::Unavailable,
        }
    }
}

const MAX_DISPLAYED_FAILURES: usize = 16;
