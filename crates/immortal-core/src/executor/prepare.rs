//! Pre-runtime command and logging materialization.
//!
//! All configuration-derived process commands, lifecycle hooks, descriptor
//! hooks, and logging pipelines are resolved before the broker forks or Tokio
//! starts. This keeps configuration and credential failures synchronous and
//! typed, and hands the broker a complete descriptor/logging ownership plan.

use std::{ffi::OsString, io, path::Path, time::Duration};

use super::{
    BrokerFileRoute, BrokerLifetimePlan, BrokerLoggerId, BrokerLoggingPlan, ExecutorError,
    FileLogConfig, LoggingPlan, ProcessCommand, RestartPolicy, ServiceConfig,
};

pub(super) fn prepare_execution(
    config: &ServiceConfig,
    controlled: bool,
) -> Result<PreparedLaunch, ExecutorError> {
    validate_supported(config, controlled)?;
    let service = ProcessCommand::from_service(config, std::env::vars_os())?;
    let condition = config
        .start_condition
        .as_ref()
        .map(|condition| ProcessCommand::from_lifecycle(&condition.command, &service))
        .transpose()?;
    let post_exit = config
        .post_exit
        .as_ref()
        .map(|hook| ProcessCommand::from_lifecycle(&hook.command, &service))
        .transpose()?;
    let descriptor_stop = config
        .descriptor_tracking
        .as_ref()
        .map(|tracking| ProcessCommand::from_lifecycle(&tracking.stop.command, &service))
        .transpose()?;
    let descriptor_reload = config
        .descriptor_tracking
        .as_ref()
        .map(|tracking| ProcessCommand::from_lifecycle(&tracking.reload.command, &service))
        .transpose()?;
    let (logging, loggers) = prepare_logging(config, &service)?;
    Ok(PreparedLaunch {
        loggers,
        logging,
        execution: PreparedExecution {
            condition,
            descriptor_reload,
            descriptor_stop,
            post_exit,
            service,
        },
    })
}

pub(super) struct PreparedLaunch {
    pub(super) execution: PreparedExecution,
    pub(super) loggers: Vec<BrokerLoggerId>,
    pub(super) logging: BrokerLoggingPlan,
}

pub(super) struct PreparedExecution {
    pub(super) condition: Option<ProcessCommand>,
    pub(super) descriptor_reload: Option<ProcessCommand>,
    pub(super) descriptor_stop: Option<ProcessCommand>,
    pub(super) post_exit: Option<ProcessCommand>,
    pub(super) service: ProcessCommand,
}

pub(super) fn prepare_logging(
    config: &ServiceConfig,
    service: &ProcessCommand,
) -> Result<(BrokerLoggingPlan, Vec<BrokerLoggerId>), ExecutorError> {
    let plan = LoggingPlan::from_config(&config.logging)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    if plan.local_files.is_empty() && plan.logger.is_none() {
        return Ok((BrokerLoggingPlan::default(), Vec::new()));
    }
    let adapter = if plan.local_files.is_empty() {
        None
    } else {
        Some(logger_adapter_program(
            config.logging.file_adapter.as_deref(),
        )?)
    };
    let has_logger = plan.logger.is_some();
    let logger = plan
        .logger
        .map(|command| ProcessCommand::from_lifecycle(&command, service))
        .transpose()?;
    let mut local_files = Vec::with_capacity(plan.local_files.len());
    let mut loggers = Vec::new();
    if logger.is_some() {
        loggers.push(BrokerLoggerId::shared_logger());
    }
    for (route_index, route) in plan.local_files.into_iter().enumerate() {
        let route_id = u16::try_from(route_index)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "too many log routes"))?;
        let adapter = adapter
            .as_ref()
            .ok_or_else(|| io::Error::other("file adapter program is absent"))?;
        local_files.push(BrokerFileRoute {
            stream: route.stream,
            command: prepare_file_adapter(route.file, has_logger, adapter, service)?,
        });
        loggers.push(BrokerLoggerId::new(route_id, 0));
    }
    Ok((
        BrokerLoggingPlan {
            local_files,
            logger,
        },
        loggers,
    ))
}

pub(super) fn logger_adapter_program(configured: Option<&Path>) -> io::Result<OsString> {
    if let Some(configured) = configured {
        return Ok(configured.as_os_str().to_owned());
    }
    let executable = std::env::current_exe()?;
    let directory = executable.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "immortal executable has no parent directory",
        )
    })?;
    Ok(directory.join("immortallog").into_os_string())
}

pub(super) fn prepare_file_adapter(
    config: FileLogConfig,
    has_downstream: bool,
    adapter: &OsString,
    service: &ProcessCommand,
) -> io::Result<ProcessCommand> {
    let mut arguments = Vec::new();
    push_logger_limit(&mut arguments, "--max-age", config.max_age_seconds);
    push_logger_limit(&mut arguments, "--keep", config.keep.map(u64::from));
    push_logger_limit(&mut arguments, "--max-bytes", config.max_bytes);
    if config.timestamp {
        arguments.push(OsString::from("--timestamp"));
    }
    if has_downstream {
        arguments.push(OsString::from("--passthrough"));
    }
    arguments.push(config.file.into_os_string());
    ProcessCommand::from_lifecycle_os(adapter, arguments, service)
}

pub(super) fn push_logger_limit(arguments: &mut Vec<OsString>, option: &str, value: Option<u64>) {
    if let Some(value) = value {
        arguments.push(OsString::from(option));
        arguments.push(OsString::from(value.to_string()));
    }
}
pub(super) fn broker_lifetime_plan(
    config: &ServiceConfig,
    commands: &PreparedExecution,
) -> Result<Option<BrokerLifetimePlan>, ExecutorError> {
    let Some(tracking) = config.descriptor_tracking.as_ref() else {
        return Ok(None);
    };
    let stop = commands.descriptor_stop.as_ref().ok_or_else(|| {
        ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared descriptor stop command is absent",
        ))
    })?;
    Ok(Some(BrokerLifetimePlan::new(
        stop.clone(),
        Duration::from_secs(tracking.stop.timeout_seconds),
        Duration::from_secs(tracking.lifetime_timeout_seconds),
    )?))
}

pub(super) fn validate_supported(
    config: &ServiceConfig,
    controlled: bool,
) -> Result<(), ExecutorError> {
    if !config.enabled {
        return Err(ExecutorError::Unsupported("disabled service execution"));
    }
    if !config.requires.is_empty() {
        return Err(ExecutorError::Unsupported("dependencies"));
    }
    if !controlled
        && !config.restart.exit_when_done
        && matches!(
            config.restart.policy,
            RestartPolicy::Never | RestartPolicy::OnFailure
        )
    {
        return Err(ExecutorError::Unsupported(
            "a persistent childless Down state before the control loop is enabled",
        ));
    }
    Ok(())
}
