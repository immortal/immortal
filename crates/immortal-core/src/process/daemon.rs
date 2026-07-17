//! Checked daemon startup: fork, session detach, and the readiness handshake.
//!
//! `daemonize` is the sole entry point that detaches the supervisor from its
//! invoking terminal before Tokio starts. It performs the double fork, session
//! creation, working-directory change, and standard-stream redirection through
//! the canonical `fork` crate, then blocks the original invoker on a bounded
//! startup channel until the detached daemon calls [`DaemonStartup::notify_ready`]
//! or the timeout elapses. Every stage that can fail is reported as a stable
//! [`DaemonStage`]/[`DaemonFailure`] pair together with any process or process
//! group `fork` left for the caller to reap or terminate, so a failed
//! detachment never silently orphans a child.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::time::Duration;

use super::identity::{ProcessGroupId, ProcessId};

/// Stable stage in the checked daemon startup sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonStage {
    FirstFork,
    CreateSession,
    SecondFork,
    CurrentDirectory,
    Descriptors,
    Initialization,
    Handshake,
    SignalState,
}

impl Display for DaemonStage {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FirstFork => "first fork",
            Self::CreateSession => "session creation",
            Self::SecondFork => "second fork",
            Self::CurrentDirectory => "working-directory change",
            Self::Descriptors => "descriptor setup",
            Self::Initialization => "daemon initialization",
            Self::Handshake => "startup handshake",
            Self::SignalState => "signal-state reset",
        })
    }
}

/// Stable classification for checked daemon startup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonFailure {
    OperatingSystem,
    TimedOut,
    Abandoned,
    InvalidHandshake,
}

/// Residual daemon ownership after fork's bounded cleanup attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DaemonCleanup {
    intermediate: Option<ProcessId>,
    process_group: Option<ProcessGroupId>,
}

impl DaemonCleanup {
    /// Return an intermediate direct child which still requires reaping.
    #[must_use]
    pub const fn intermediate(self) -> Option<ProcessId> {
        self.intermediate
    }

    /// Return a daemon process group which may still require termination.
    #[must_use]
    pub const fn process_group(self) -> Option<ProcessGroupId> {
        self.process_group
    }

    /// Whether fork completed every cleanup obligation before returning.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.intermediate.is_none() && self.process_group.is_none()
    }
}

/// Failure to detach or initialize a checked daemon.
#[derive(Debug)]
pub struct DaemonError {
    stage: Option<DaemonStage>,
    failure: DaemonFailure,
    cleanup_pending: DaemonCleanup,
    source: Option<io::Error>,
}

impl DaemonError {
    /// Return the startup stage when the failure came from an OS operation.
    #[must_use]
    pub const fn stage(&self) -> Option<DaemonStage> {
        self.stage
    }

    /// Return the stable failure classification.
    #[must_use]
    pub const fn failure(&self) -> DaemonFailure {
        self.failure
    }

    /// Return residual process ownership after bounded cleanup.
    #[must_use]
    pub const fn cleanup_pending(&self) -> DaemonCleanup {
        self.cleanup_pending
    }

    /// Return the underlying OS error number when one was reported.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.source.as_ref().and_then(io::Error::raw_os_error)
    }
}

impl Display for DaemonError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match (self.failure, self.stage, self.source.as_ref()) {
            (DaemonFailure::OperatingSystem, Some(stage), Some(error)) => {
                write!(formatter, "daemon {stage} failed: {error}")
            }
            (DaemonFailure::OperatingSystem, Some(stage), None) => {
                write!(formatter, "daemon {stage} failed")
            }
            (DaemonFailure::OperatingSystem, None, _) => {
                formatter.write_str("daemon preparation failed")
            }
            (DaemonFailure::TimedOut, _, _) => formatter.write_str("daemon startup timed out"),
            (DaemonFailure::Abandoned, _, _) => {
                formatter.write_str("daemon abandoned its startup channel")
            }
            (DaemonFailure::InvalidHandshake, _, _) => {
                formatter.write_str("daemon returned an invalid startup handshake")
            }
        }
    }
}

impl Error for DaemonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

/// Detached daemon-side readiness token.
#[derive(Debug)]
pub struct DaemonStartup(fork::DaemonNotifier);

impl DaemonStartup {
    /// Tell the original invoker that Immortal's runtime is ready.
    ///
    /// # Errors
    ///
    /// Returns the startup-channel error when the invoker is no longer waiting.
    pub fn notify_ready(self) -> io::Result<()> {
        self.0.notify_ready()
    }

    /// Report initialization failure and terminate without inherited destructors.
    pub fn fail_and_exit(self, error: &io::Error) -> ! {
        self.0.fail_and_exit(error)
    }
}

/// Which process returned from checked daemonization.
#[derive(Debug)]
pub enum Daemonized {
    /// The original invoker after the detached supervisor reported ready.
    Parent {
        process: ProcessId,
        process_group: ProcessGroupId,
    },
    /// The detached supervisor which must initialize and report readiness.
    Daemon(DaemonStartup),
}

/// Detach before Tokio starts and retain a checked startup channel.
///
/// The supervisor changes to `/`, redirects all standard streams to `/dev/null`,
/// and resets portable signal state. Service paths and environment must be
/// materialized before calling this function.
///
/// # Errors
///
/// Returns preparation, fork/session, handshake, timeout, or cleanup failures.
pub fn daemonize(startup_timeout: Duration) -> Result<Daemonized, DaemonError> {
    let mut options = fork::DaemonOptions::new();
    options
        .current_directory("/")
        .map_err(|error| daemon_preparation_error(DaemonStage::CurrentDirectory, error))?
        .redirect_standard_io_to_null()
        .map_err(|error| daemon_preparation_error(DaemonStage::Descriptors, error))?;
    match fork::checked_daemon(options, startup_timeout).map_err(daemon_error_from_fork)? {
        fork::CheckedDaemon::Parent(process) => Ok(Daemonized::Parent {
            process: ProcessId(process.process().get()),
            process_group: ProcessGroupId(process.process_group().get()),
        }),
        fork::CheckedDaemon::Daemon(notifier) => Ok(Daemonized::Daemon(DaemonStartup(notifier))),
    }
}

fn daemon_preparation_error(stage: DaemonStage, error: io::Error) -> DaemonError {
    DaemonError {
        stage: Some(stage),
        failure: DaemonFailure::OperatingSystem,
        cleanup_pending: DaemonCleanup::default(),
        source: Some(error),
    }
}

fn daemon_error_from_fork(error: fork::DaemonError) -> DaemonError {
    let cleanup = error.cleanup_pending();
    let cleanup_pending = DaemonCleanup {
        intermediate: cleanup
            .intermediate()
            .map(|process| ProcessId(process.get())),
        process_group: cleanup
            .process_group()
            .map(|group| ProcessGroupId(group.get())),
    };
    match error {
        fork::DaemonError::OperatingSystem { stage, error, .. } => DaemonError {
            stage: Some(daemon_stage_from_fork(stage)),
            failure: DaemonFailure::OperatingSystem,
            cleanup_pending,
            source: Some(error),
        },
        fork::DaemonError::TimedOut { .. } => DaemonError {
            stage: None,
            failure: DaemonFailure::TimedOut,
            cleanup_pending,
            source: None,
        },
        fork::DaemonError::Abandoned { .. } => DaemonError {
            stage: None,
            failure: DaemonFailure::Abandoned,
            cleanup_pending,
            source: None,
        },
        fork::DaemonError::InvalidHandshake { .. } => DaemonError {
            stage: None,
            failure: DaemonFailure::InvalidHandshake,
            cleanup_pending,
            source: None,
        },
    }
}

const fn daemon_stage_from_fork(stage: fork::DaemonStage) -> DaemonStage {
    match stage {
        fork::DaemonStage::FirstFork => DaemonStage::FirstFork,
        fork::DaemonStage::CreateSession => DaemonStage::CreateSession,
        fork::DaemonStage::SecondFork => DaemonStage::SecondFork,
        fork::DaemonStage::CurrentDirectory => DaemonStage::CurrentDirectory,
        fork::DaemonStage::Descriptors => DaemonStage::Descriptors,
        fork::DaemonStage::Initialization => DaemonStage::Initialization,
        fork::DaemonStage::Handshake => DaemonStage::Handshake,
        fork::DaemonStage::SignalState => DaemonStage::SignalState,
    }
}
