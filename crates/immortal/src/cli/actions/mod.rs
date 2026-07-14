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
    io::{self, Write},
    path::{Path, PathBuf},
};

use immortal_core::{
    config::{
        ConfigError, ServiceConfig, emit_config, load_environment_directory, parse_file,
        resolve_paths,
    },
    executor::{
        DaemonRunOutcome, ExecutorError, SupervisionOutcome, run_daemon, run_foreground_controlled,
    },
    exit::ExitClass,
    runtime::prepare_user_service_directory,
    supervisor::SupervisorState,
};

use crate::cli::dispatch::{Action, DirectService, RuntimeIdentity};

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
            let control_directory = match control_directory {
                Some(directory) => directory,
                None => config_runtime_directory(&path)?,
            };
            supervise(&config, &control_directory, foreground)
        }
        Action::SuperviseCommand(service) => {
            let (config, runtime_identity, foreground) = direct_config(service)?;
            let control_directory = match runtime_identity {
                RuntimeIdentity::Name(name) => {
                    prepare_user_service_directory(&name).map_err(ActionError::Runtime)?
                }
                RuntimeIdentity::ControlDirectory(directory) => directory,
            };
            supervise(&config, &control_directory, foreground)
        }
    }
}

fn supervise(
    config: &ServiceConfig,
    control_directory: &Path,
    foreground: bool,
) -> Result<(), ActionError> {
    if foreground {
        let outcome = run_foreground_controlled(config, control_directory)?;
        finish_supervision(outcome)
    } else {
        match run_daemon(config, Some(control_directory))? {
            DaemonRunOutcome::Parent => Ok(()),
            DaemonRunOutcome::Daemon(outcome) => finish_supervision(outcome),
        }
    }
}

fn config_runtime_directory(path: &Path) -> Result<PathBuf, ActionError> {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ActionError::Runtime(io::Error::new(
                io::ErrorKind::InvalidInput,
                "configuration filename has no UTF-8 service stem",
            ))
        })?;
    prepare_user_service_directory(name).map_err(ActionError::Runtime)
}

fn direct_config(
    service: DirectService,
) -> Result<(ServiceConfig, RuntimeIdentity, bool), ActionError> {
    let DirectService {
        child_pid,
        command,
        environment_directory,
        foreground,
        retries,
        runtime_identity,
        start_delay_seconds,
        supervisor_pid,
        user,
        working_directory,
    } = service;
    let mut config = ServiceConfig::for_command(command)?;
    config.restart.limits.max_retries = if retries < 0 {
        None
    } else {
        Some(u32::try_from(retries).map_err(|_| {
            ConfigError::Validation(vec!["retries must be -1 or a nonnegative count".to_owned()])
        })?)
    };
    config.start_delay_seconds = start_delay_seconds;
    config.pid_files.main = child_pid;
    config.pid_files.supervisor = supervisor_pid;
    if let Some(directory) = environment_directory {
        config.environment = load_environment_directory(&directory)?;
    }
    config.user = user;
    config.working_directory = working_directory;
    let base = std::env::current_dir().map_err(ActionError::Output)?;
    resolve_paths(&mut config, &base)?;
    Ok((config, runtime_identity, foreground))
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
