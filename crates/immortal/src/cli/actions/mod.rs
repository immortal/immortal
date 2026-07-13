//! Coordination of application operations selected by CLI dispatch.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
};

use immortal_core::{
    config::{ConfigError, ServiceConfig, emit_config, parse_file, resolve_paths},
    executor::{
        DaemonRunOutcome, ExecutorError, SupervisionOutcome, run_daemon, run_foreground,
        run_foreground_controlled,
    },
    exit::ExitClass,
    supervisor::SupervisorState,
};

use crate::cli::dispatch::{Action, DirectService};

/// Failure while coordinating an application action.
#[derive(Debug)]
pub enum ActionError {
    /// Configuration parsing, normalization, or emission failed.
    Config(ConfigError),
    /// Normalized output could not be written.
    Output(io::Error),
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
            Self::Executor(error) => Display::fmt(error, formatter),
            Self::ServiceFailed(outcome) => write!(
                formatter,
                "service supervision stopped in {:?} after {} start(s); last result {:?}",
                outcome.state, outcome.starts, outcome.last_result
            ),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Output(error) => Some(error),
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
    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::Config(_) => ExitClass::Configuration,
            Self::Output(_) => ExitClass::IoError,
            Self::Executor(ExecutorError::Unsupported(_)) => ExitClass::Unavailable,
            Self::Executor(ExecutorError::OperatingSystem(_) | ExecutorError::Daemon(_)) => {
                ExitClass::OsError
            }
            Self::Executor(_) => ExitClass::Software,
            Self::ServiceFailed(_) => ExitClass::TemporaryFailure,
        }
    }
}

/// Execute one typed action.
///
/// # Errors
///
/// Returns an error when input is invalid, output fails, or a requested
/// operational capability has not yet passed its implementation gate.
pub fn execute(action: Action) -> Result<(), ActionError> {
    match action {
        Action::CheckConfig(path) => {
            let config = parse_file(&path)?;
            let output = emit_config(&config)?;
            io::stdout().lock().write_all(output.as_bytes())?;
            Ok(())
        }
        Action::SuperviseConfig {
            control_directory,
            path,
            foreground,
        } => {
            let config = parse_file(&path)?;
            supervise(&config, control_directory.as_deref(), foreground)
        }
        Action::SuperviseCommand(service) => {
            let foreground = service.foreground;
            let control_directory = service.control_directory.clone();
            let config = direct_config(service)?;
            supervise(&config, control_directory.as_deref(), foreground)
        }
    }
}

fn supervise(
    config: &ServiceConfig,
    control_directory: Option<&std::path::Path>,
    foreground: bool,
) -> Result<(), ActionError> {
    if foreground {
        let outcome = control_directory.map_or_else(
            || run_foreground(config),
            |directory| run_foreground_controlled(config, directory),
        )?;
        finish_supervision(outcome)
    } else {
        match run_daemon(config, control_directory)? {
            DaemonRunOutcome::Parent => Ok(()),
            DaemonRunOutcome::Daemon(outcome) => finish_supervision(outcome),
        }
    }
}

fn direct_config(service: DirectService) -> Result<ServiceConfig, ActionError> {
    let mut config = ServiceConfig::for_command(service.command)?;
    config.restart.limits.max_retries = if service.retries < 0 {
        None
    } else {
        Some(u32::try_from(service.retries).map_err(|_| {
            ConfigError::Validation(vec!["retries must be -1 or a nonnegative count".to_owned()])
        })?)
    };
    config.start_delay_seconds = service.start_delay_seconds;
    config.pid_files.main = service.child_pid;
    config.pid_files.supervisor = service.supervisor_pid;
    config.user = service.user;
    config.working_directory = service.working_directory;
    let base = std::env::current_dir().map_err(ActionError::Output)?;
    resolve_paths(&mut config, &base)?;
    Ok(config)
}

fn finish_supervision(outcome: SupervisionOutcome) -> Result<(), ActionError> {
    if outcome.last_start_failed
        || outcome.last_readiness_failed
        || matches!(outcome.state, SupervisorState::Failed(_))
    {
        Err(ActionError::ServiceFailed(outcome))
    } else {
        Ok(())
    }
}
