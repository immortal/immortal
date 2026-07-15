//! Materialization and supervision for direct command-line services.
//!
//! Typed CLI values are converted into the same validated `ServiceConfig` used
//! by file-based execution. Environment-directory data and relative paths are
//! resolved before shared supervision can create brokers or daemonize.

use immortal_core::{
    config::{
        ConfigError, LoggingConfig, ServiceConfig, load_environment_directory, resolve_paths,
    },
    runtime::prepare_user_service_directory,
};

use super::{ActionError, DirectService, RuntimeIdentity, supervision};

/// Convert direct CLI values into a service definition and supervise it.
///
/// # Errors
///
/// Returns an error when configuration materialization, runtime preparation, or
/// supervision fails.
pub fn execute(service: DirectService) -> Result<(), ActionError> {
    let (config, runtime_identity, foreground) = direct_config(service)?;
    let control_directory = match runtime_identity {
        RuntimeIdentity::Name(name) => {
            prepare_user_service_directory(&name).map_err(ActionError::Runtime)?
        }
        RuntimeIdentity::ControlDirectory(directory) => directory,
    };
    supervision::execute(&config, &control_directory, foreground)
}

fn direct_config(
    service: DirectService,
) -> Result<(ServiceConfig, RuntimeIdentity, bool), ActionError> {
    let DirectService {
        child_pid,
        command,
        environment_directory,
        foreground,
        logfile,
        logger,
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
    config.logging = LoggingConfig::for_direct(logfile, logger)?;
    if let Some(directory) = environment_directory {
        config.environment = load_environment_directory(&directory)?;
    }
    config.user = user;
    config.working_directory = working_directory;
    let base = std::env::current_dir().map_err(ActionError::Output)?;
    resolve_paths(&mut config, &base)?;
    Ok((config, runtime_identity, foreground))
}
