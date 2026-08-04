//! Materialization and supervision for direct command-line services.
//!
//! Typed CLI values are converted into the same validated `ServiceConfig` used
//! by file-based execution. Environment-directory data and relative paths are
//! resolved before shared supervision can create brokers or daemonize.

use immortal_core::{
    config::{
        ConfigError, LoggingConfig, ServiceConfig, load_environment_directory, resolve_paths,
        validate_service,
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
    // `for_command` only validated the defaults it built. Every field assigned
    // above bypassed that check, so the fully materialized definition is
    // validated here before it can reach process setup.
    validate_service(&config)?;
    Ok((config, runtime_identity, foreground))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use immortal_core::config::MAX_SCHEDULE_SECONDS;

    use super::{ActionError, DirectService, RuntimeIdentity, direct_config};

    fn service() -> DirectService {
        DirectService {
            child_pid: None,
            command: vec!["/bin/true".to_owned()],
            environment_directory: None,
            foreground: true,
            logfile: None,
            logger: None,
            retries: -1,
            runtime_identity: RuntimeIdentity::Name("probe".to_owned()),
            start_delay_seconds: 0,
            supervisor_pid: None,
            user: None,
            working_directory: None,
        }
    }

    /// Assert one post-construction field is rejected by configuration validation.
    fn assert_rejected(service: DirectService, field: &str) -> Result<(), Box<dyn Error>> {
        match direct_config(service) {
            Err(ActionError::Config(_)) => Ok(()),
            Err(error) => Err(format!("{field} produced an unexpected error: {error}").into()),
            Ok(_) => Err(format!("an invalid {field} reached supervision unvalidated").into()),
        }
    }

    #[test]
    fn direct_config_accepts_a_fully_populated_definition() -> Result<(), Box<dyn Error>> {
        let mut service = service();
        service.start_delay_seconds = MAX_SCHEDULE_SECONDS;
        service.retries = 3;
        service.child_pid = Some(PathBuf::from("/tmp/immortal-probe-child.pid"));
        service.supervisor_pid = Some(PathBuf::from("/tmp/immortal-probe-supervisor.pid"));
        service.working_directory = Some(PathBuf::from("/tmp"));
        let (config, identity, foreground) = direct_config(service)?;
        assert_eq!(config.start_delay_seconds, MAX_SCHEDULE_SECONDS);
        assert_eq!(config.restart.limits.max_retries, Some(3));
        assert_eq!(identity, RuntimeIdentity::Name("probe".to_owned()));
        assert!(foreground);
        Ok(())
    }

    #[test]
    fn direct_config_accepts_no_start_delay() -> Result<(), Box<dyn Error>> {
        let (config, _, _) = direct_config(service())?;
        assert_eq!(config.start_delay_seconds, 0);
        Ok(())
    }

    #[test]
    fn direct_config_rejects_a_start_delay_beyond_the_schedule_bound() -> Result<(), Box<dyn Error>>
    {
        let mut service = service();
        service.start_delay_seconds = u64::MAX;
        assert_rejected(service, "start_delay_seconds")
    }

    #[test]
    fn direct_config_rejects_an_unusable_working_directory() -> Result<(), Box<dyn Error>> {
        let mut service = service();
        service.working_directory = Some(PathBuf::from("/bin/sh"));
        assert_rejected(service, "working_directory")
    }

    #[test]
    fn direct_config_rejects_an_empty_user() -> Result<(), Box<dyn Error>> {
        let mut service = service();
        service.user = Some(String::new());
        assert_rejected(service, "user")
    }

    #[test]
    fn direct_config_resolves_pid_files_before_validating_them() -> Result<(), Box<dyn Error>> {
        let mut service = service();
        service.child_pid = Some(PathBuf::from("child.pid"));
        service.supervisor_pid = Some(PathBuf::from("supervisor.pid"));
        let base = std::env::current_dir()?;
        let (config, _, _) = direct_config(service)?;
        assert_eq!(config.pid_files.main, Some(base.join("child.pid")));
        assert_eq!(
            config.pid_files.supervisor,
            Some(base.join("supervisor.pid"))
        );
        Ok(())
    }

    #[test]
    fn direct_config_rejects_an_empty_logger_command() -> Result<(), Box<dyn Error>> {
        let mut service = service();
        service.logger = Some(vec![String::new()]);
        assert_rejected(service, "logger")
    }

    #[test]
    fn direct_config_rejects_an_invalid_command() -> Result<(), Box<dyn Error>> {
        let mut service = service();
        service.command = vec![String::new()];
        assert_rejected(service, "command")
    }
}
