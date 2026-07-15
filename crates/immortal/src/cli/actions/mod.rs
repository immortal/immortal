//! Coordination of fully typed `immortal` application operations.
//!
//! Configuration is parsed and direct inputs are materialized before runtime
//! identity is resolved. Exact control directories are preserved, while config
//! stems and direct names prepare the effective user's runtime root through
//! `immortal-core`. Only then may foreground execution or checked daemonization
//! acquire service ownership and start process infrastructure.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use immortal_core::{
    config::ConfigError,
    executor::{ExecutorError, SupervisionOutcome},
    exit::ExitClass,
};

pub mod check_config;
pub mod supervise_command;
pub mod supervise_config;

mod supervision;

/// Typed direct-command inputs which are safe to apply before broker creation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectService {
    pub child_pid: Option<PathBuf>,
    pub command: Vec<String>,
    pub environment_directory: Option<PathBuf>,
    pub foreground: bool,
    pub logfile: Option<PathBuf>,
    pub logger: Option<Vec<String>>,
    pub retries: i32,
    pub runtime_identity: RuntimeIdentity,
    pub start_delay_seconds: u64,
    pub supervisor_pid: Option<PathBuf>,
    pub user: Option<String>,
    pub working_directory: Option<PathBuf>,
}

/// Exclusive runtime identity selected for one direct command.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeIdentity {
    /// Service name resolved below the effective user's runtime root.
    Name(String),
    /// Exact absolute runtime service directory.
    ControlDirectory(PathBuf),
}

/// Typed operation selected by the command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Validate a file and emit the normalized supported schema.
    CheckConfig(PathBuf),
    /// Supervise the service described by a configuration file.
    SuperviseConfig {
        control_directory: Option<PathBuf>,
        path: PathBuf,
        foreground: bool,
    },
    /// Supervise a direct argv command.
    SuperviseCommand(DirectService),
}

/// Failure while coordinating an application action.
#[derive(Debug)]
pub enum ActionError {
    /// Configuration parsing, normalization, or emission failed.
    Config(ConfigError),
    /// Normalized output could not be written.
    Output(io::Error),
    /// Automatic user runtime identity could not be prepared safely.
    Runtime(io::Error),
    /// Foreground process execution failed.
    Executor(ExecutorError),
    /// Supervision stopped in a configured terminal failure state.
    ServiceFailed(SupervisionOutcome),
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => Display::fmt(error, formatter),
            Self::Output(error) => write!(formatter, "unable to write output: {error}"),
            Self::Runtime(error) => write!(formatter, "unable to prepare service runtime: {error}"),
            Self::Executor(error) => Display::fmt(error, formatter),
            Self::ServiceFailed(outcome) => {
                if let Some(reason) = outcome.terminal_failure {
                    write!(
                        formatter,
                        "service supervision exited after {} start(s) because {reason:?}; last result {:?}",
                        outcome.starts, outcome.last_result
                    )
                } else {
                    write!(
                        formatter,
                        "service supervision stopped in {:?} after {} start(s); last result {:?}",
                        outcome.state, outcome.starts, outcome.last_result
                    )
                }
            }
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Output(error) | Self::Runtime(error) => Some(error),
            Self::Executor(error) => Some(error),
            Self::ServiceFailed(_) => None,
        }
    }
}

impl From<ConfigError> for ActionError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for ActionError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

impl From<ExecutorError> for ActionError {
    fn from(error: ExecutorError) -> Self {
        Self::Executor(error)
    }
}

impl ActionError {
    /// Stable process exit classification for this failure.
    #[must_use]
    pub fn exit_class(&self) -> ExitClass {
        match self {
            Self::Config(_) => ExitClass::Configuration,
            Self::Output(_) => ExitClass::IoError,
            Self::Runtime(error) if error.kind() == io::ErrorKind::InvalidInput => {
                ExitClass::Configuration
            }
            Self::Runtime(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                ExitClass::Permission
            }
            Self::Runtime(_) => ExitClass::CantCreate,
            Self::Executor(ExecutorError::Unsupported(_)) => ExitClass::Unavailable,
            Self::Executor(ExecutorError::OperatingSystem(_) | ExecutorError::Daemon(_)) => {
                ExitClass::OsError
            }
            Self::Executor(_) => ExitClass::Software,
            Self::ServiceFailed(_) => ExitClass::TemporaryFailure,
        }
    }
}
