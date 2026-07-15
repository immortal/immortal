//! Single-owner service supervision driven through the process broker.
//!
//! Preparation resolves every fallible command and credential input before a
//! broker or Tokio runtime exists. The executor then owns the lifecycle state,
//! logger stages, deadlines, and control-command serialization while the
//! pre-runtime broker exclusively owns Unix children and wait operations.
//! Controlled execution exposes `Initializing` after binding its authenticated
//! socket and keeps it until asynchronous logger stages are ready. Every exit
//! path drains or terminates owned logger groups, shuts down the broker, and
//! reaps it before returning; no task or PID is detached implicitly.
//! The one shared external logger starts before local file adapters. Shutdown
//! reverses data ownership rather than process order: adapters drain and lose
//! their shared-pipe writers before the external logger receives EOF and its own
//! bounded escalation interval.
//! Descriptor tracking keeps logical lifetime separate from launcher identity:
//! stop/reload hooks remain serialized here while descriptor ownership and
//! supervisor-loss fallback remain inside the broker.

use std::{
    collections::VecDeque,
    error::Error,
    ffi::OsString,
    fmt::{self, Display, Formatter},
    future::pending,
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{
    runtime::Builder,
    sync::{mpsc, watch},
    time::{Instant as TokioInstant, timeout},
};

use crate::{
    config::{
        FileLogConfig, LoggerRestartConfig, ProcessMode, ReadinessMode, RestartPolicy,
        ServiceConfig, StartConditionConfig,
    },
    control::{
        ControlCommand, ControlEffect, ControlListener, DEFAULT_MAX_CONTROL_CLIENTS, Operation,
        Response, ResponseCode, Signal, SignalScope, StopCompletion, decide_request,
        run_control_server,
    },
    logging::LoggingPlan,
    pid_file::OwnedPidFile,
    process::{
        BrokerFileRoute, BrokerLifetimePlan, BrokerLoggerId, BrokerLoggingPlan, BrokerSignalScope,
        BrokerTaskId, ChildEvent, DaemonError, DaemonStartup, Daemonized, ProcessBrokerClient,
        ProcessBrokerError, ProcessBrokerEvent, ProcessCommand, ProcessGroupId, ProcessId,
        ProcessSignal, SignalTarget, daemonize, reap_any_event, signal,
        start_process_broker_with_logging, wait_for_event,
    },
    runtime::RuntimeOwner,
    shutdown::TerminationSignals,
    status::{LastResult, LoggerStatus, StatusSnapshot},
    supervisor::{
        ChildResult, ConditionTracker, DesiredState, FailureReason, Generation, RestartDecision,
        RestartTracker, StateMachine, SupervisorState, TransitionError,
    },
};

const BROKER_EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SERVICE_STOP_GRACE: Duration = Duration::from_secs(5);
const LOGGER_PREPARE_GRACE: Duration = Duration::from_secs(5);
const LOGGER_DRAIN_GRACE: Duration = Duration::from_secs(5);
const LOGGER_STOP_GRACE: Duration = Duration::from_secs(2);
const SPAWN_FAILURE_EXIT: u8 = 127;

/// Final observation returned by the foreground executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisionOutcome {
    /// Terminal supervisor state reached by this foreground invocation.
    pub state: SupervisorState,
    /// Restart-limit failure which requested terminal cleanup instead of persistent failure.
    pub terminal_failure: Option<FailureReason>,
    /// Last reaped generation result, absent when shutdown preceded the first start.
    pub last_result: Option<ChildResult>,
    /// Whether the last recorded result represents a failed exec handshake.
    pub last_start_failed: bool,
    /// Whether the last generation failed its descriptor readiness contract.
    pub last_readiness_failed: bool,
    /// Number of generation attempts, including failed exec handshakes.
    pub starts: u64,
}

/// Result observed by a process which participates in checked daemon startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonRunOutcome {
    /// The original invoking process may return success to its caller.
    Parent,
    /// The detached supervisor eventually completed its lifecycle.
    Daemon(SupervisionOutcome),
}

/// Failure to initialize or drive one foreground supervisor.
#[derive(Debug)]
pub enum ExecutorError {
    Unsupported(&'static str),
    OperatingSystem(io::Error),
    Daemon(DaemonError),
    Broker(ProcessBrokerError),
    Transition(TransitionError),
    BrokerTimedOut(&'static str),
    ContainmentFailed(ProcessBrokerEvent),
    UnexpectedBrokerEvent(ProcessBrokerEvent),
    UnexpectedChildEvent(ChildEvent),
    BrokerExited(ChildEvent),
    ControlServerStopped,
}

impl Display for ExecutorError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(capability) => {
                write!(
                    formatter,
                    "foreground supervision does not yet support {capability}"
                )
            }
            Self::OperatingSystem(error) => Display::fmt(error, formatter),
            Self::Daemon(error) => Display::fmt(error, formatter),
            Self::Broker(error) => Display::fmt(error, formatter),
            Self::Transition(error) => Display::fmt(error, formatter),
            Self::BrokerTimedOut(operation) => {
                write!(formatter, "process broker timed out during {operation}")
            }
            Self::ContainmentFailed(event) => {
                write!(formatter, "process-group containment failed: {event:?}")
            }
            Self::UnexpectedBrokerEvent(event) => {
                write!(formatter, "unexpected process broker event: {event:?}")
            }
            Self::UnexpectedChildEvent(event) => {
                write!(formatter, "unexpected direct child event: {event:?}")
            }
            Self::BrokerExited(event) => {
                write!(formatter, "process broker exited unsuccessfully: {event:?}")
            }
            Self::ControlServerStopped => {
                formatter.write_str("authenticated control server stopped unexpectedly")
            }
        }
    }
}

impl Error for ExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OperatingSystem(error) => Some(error),
            Self::Daemon(error) => Some(error),
            Self::Broker(error) => Some(error),
            Self::Transition(error) => Some(error),
            Self::Unsupported(_)
            | Self::BrokerTimedOut(_)
            | Self::ContainmentFailed(_)
            | Self::UnexpectedBrokerEvent(_)
            | Self::UnexpectedChildEvent(_)
            | Self::BrokerExited(_)
            | Self::ControlServerStopped => None,
        }
    }
}

impl From<io::Error> for ExecutorError {
    fn from(error: io::Error) -> Self {
        Self::OperatingSystem(error)
    }
}

impl From<ProcessBrokerError> for ExecutorError {
    fn from(error: ProcessBrokerError) -> Self {
        Self::Broker(error)
    }
}

impl From<DaemonError> for ExecutorError {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(error)
    }
}

impl From<TransitionError> for ExecutorError {
    fn from(error: TransitionError) -> Self {
        Self::Transition(error)
    }
}

/// Run one service through a broker created before the current-thread Tokio runtime.
///
/// The executor owns service groups, readiness, hooks, logger chains, restart
/// policy, bounded backoff, and complete broker shutdown. Configuration which
/// requires a directory manager or persistent control owner fails closed
/// through [`ExecutorError::Unsupported`].
///
/// # Errors
///
/// Returns configuration-capability, process, broker, or state-transition failures.
pub fn run_foreground(config: &ServiceConfig) -> Result<SupervisionOutcome, ExecutorError> {
    let commands = prepare_execution(config, false)?;
    run_prepared(config, None, commands, &mut StartupReporter::Foreground)
}

/// Run one foreground service while exclusively owning an authenticated control endpoint.
///
/// `directory` is the exact absolute `ROOT/SERVICE` runtime directory. Its
/// parent must already exist and satisfy the runtime-root permission contract.
/// The ownership lock is acquired before the process broker is forked, and is
/// retained until the broker has completed child cleanup.
///
/// # Errors
///
/// Returns configuration, runtime ownership, control-socket, broker, process,
/// or lifecycle failures.
pub fn run_foreground_controlled(
    config: &ServiceConfig,
    directory: &Path,
) -> Result<SupervisionOutcome, ExecutorError> {
    let owner = RuntimeOwner::acquire(directory)?;
    let service_name = owner
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid service name"))?
        .to_owned();
    let commands = prepare_execution(config, true)?;
    run_prepared(
        config,
        Some(ControlSetup {
            owner,
            service_name,
        }),
        commands,
        &mut StartupReporter::Foreground,
    )
}

/// Detach one fully materialized service before creating Tokio or the broker.
///
/// The original invoker returns only after the detached child has acquired any
/// configured runtime ownership, started the broker and runtime, and bound the
/// authenticated control listener. The detached child continues supervision.
///
/// # Errors
///
/// Returns preparation or checked daemon errors to the original invoker. The
/// detached child reports initialization failures through the startup channel.
pub fn run_daemon(
    config: &ServiceConfig,
    control_directory: Option<&Path>,
) -> Result<DaemonRunOutcome, ExecutorError> {
    let commands = prepare_execution(config, control_directory.is_some())?;
    match daemonize(DAEMON_STARTUP_TIMEOUT)? {
        Daemonized::Parent { .. } => Ok(DaemonRunOutcome::Parent),
        Daemonized::Daemon(notifier) => {
            let mut startup = StartupReporter::Daemon(Some(notifier));
            let execution = (|| {
                let control = control_directory.map(ControlSetup::acquire).transpose()?;
                run_prepared(config, control, commands, &mut startup)
            })();
            if let Err(error) = &execution {
                startup.fail_if_pending(error);
            }
            execution.map(DaemonRunOutcome::Daemon)
        }
    }
}

struct ControlSetup {
    owner: RuntimeOwner,
    service_name: String,
}

impl ControlSetup {
    fn acquire(directory: &Path) -> Result<Self, ExecutorError> {
        let owner = RuntimeOwner::acquire(directory)?;
        let service_name = owner
            .directory()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid service name"))?
            .to_owned();
        Ok(Self {
            owner,
            service_name,
        })
    }
}

enum StartupReporter {
    Foreground,
    Daemon(Option<DaemonStartup>),
}

impl StartupReporter {
    fn notify_ready(&mut self) -> Result<(), ExecutorError> {
        match self {
            Self::Foreground => Ok(()),
            Self::Daemon(notifier) => notifier
                .take()
                .ok_or_else(|| io::Error::other("daemon readiness was already reported"))?
                .notify_ready()
                .map_err(Into::into),
        }
    }

    fn fail_if_pending(&mut self, error: &ExecutorError) {
        let Self::Daemon(notifier) = self else {
            return;
        };
        if let Some(notifier) = notifier.take() {
            notifier.fail_and_exit(&startup_io_error(error));
        }
    }
}

fn startup_io_error(error: &ExecutorError) -> io::Error {
    let mut current: &(dyn Error + 'static) = error;
    loop {
        if let Some(error) = current.downcast_ref::<io::Error>() {
            return error.raw_os_error().map_or_else(
                || io::Error::new(error.kind(), error.to_string()),
                io::Error::from_raw_os_error,
            );
        }
        let Some(source) = current.source() else {
            return io::Error::other(error.to_string());
        };
        current = source;
    }
}

fn prepare_execution(
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

struct PreparedLaunch {
    execution: PreparedExecution,
    loggers: Vec<BrokerLoggerId>,
    logging: BrokerLoggingPlan,
}

struct PreparedExecution {
    condition: Option<ProcessCommand>,
    descriptor_reload: Option<ProcessCommand>,
    descriptor_stop: Option<ProcessCommand>,
    post_exit: Option<ProcessCommand>,
    service: ProcessCommand,
}

fn prepare_logging(
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

fn logger_adapter_program(configured: Option<&Path>) -> io::Result<OsString> {
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

fn prepare_file_adapter(
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

fn push_logger_limit(arguments: &mut Vec<OsString>, option: &str, value: Option<u64>) {
    if let Some(value) = value {
        arguments.push(OsString::from(option));
        arguments.push(OsString::from(value.to_string()));
    }
}

fn run_prepared(
    config: &ServiceConfig,
    control: Option<ControlSetup>,
    prepared: PreparedLaunch,
    startup: &mut StartupReporter,
) -> Result<SupervisionOutcome, ExecutorError> {
    let _supervisor_pid_file = config
        .pid_files
        .supervisor
        .as_deref()
        .map(|path| OwnedPidFile::publish(path, std::process::id()))
        .transpose()?;
    let lifetime_cleanup = broker_lifetime_plan(config, &prepared.execution)?;
    let endpoint = start_process_broker_with_logging(prepared.logging, lifetime_cleanup)?;
    let commands = prepared.execution;
    let loggers = prepared.loggers;
    let broker_process = endpoint.process();
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let execution = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        wait_for_ready(&mut client).await?;
        let mut signals = TerminationSignals::new()?;
        let outcome = if let Some(setup) = control {
            drive_controlled_service(
                &mut client,
                commands,
                loggers,
                config,
                &mut signals,
                setup,
                startup,
            )
            .await
        } else {
            startup.notify_ready()?;
            drive_service(
                &mut client,
                commands,
                loggers,
                config,
                &mut signals,
                None,
                None,
            )
            .await
        };
        let shutdown = shutdown_broker(&mut client).await;
        match (outcome, shutdown) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    });
    drop(runtime);
    let broker_exit = reap_broker(broker_process);
    match (execution, broker_exit) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}

fn broker_lifetime_plan(
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

fn validate_supported(config: &ServiceConfig, controlled: bool) -> Result<(), ExecutorError> {
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

async fn drive_controlled_service(
    client: &mut ProcessBrokerClient,
    commands: PreparedExecution,
    loggers: Vec<BrokerLoggerId>,
    config: &ServiceConfig,
    signals: &mut TerminationSignals,
    setup: ControlSetup,
    startup: &mut StartupReporter,
) -> Result<SupervisionOutcome, ExecutorError> {
    let listener = Arc::new(ControlListener::bind(
        setup.owner.socket(),
        DEFAULT_MAX_CONTROL_CLIENTS,
    )?);
    let (command_sender, mut control_commands) = mpsc::channel(DEFAULT_MAX_CONTROL_CLIENTS);
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let server_listener = Arc::clone(&listener);
    let server = tokio::spawn(run_control_server(
        server_listener,
        command_sender,
        shutdown_receiver,
    ));
    startup.notify_ready()?;
    let outcome = drive_service(
        client,
        commands,
        loggers,
        config,
        signals,
        Some(&mut control_commands),
        Some(setup.service_name.as_str()),
    )
    .await;
    let _ = shutdown_sender.send(true);
    let server_result = server
        .await
        .map_err(|error| io::Error::other(format!("control server task failed: {error}")))?;
    drop(listener);
    drop(setup.owner);
    match (outcome, server_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(io::Error::other(error).into()),
    }
}

async fn wait_for_ready(client: &mut ProcessBrokerClient) -> Result<(), ExecutorError> {
    match timeout(BROKER_EVENT_TIMEOUT, client.next_event()).await {
        Ok(Ok(ProcessBrokerEvent::Ready)) => Ok(()),
        Ok(Ok(event)) => Err(ExecutorError::UnexpectedBrokerEvent(event)),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(ExecutorError::BrokerTimedOut("startup")),
    }
}

async fn drive_service(
    client: &mut ProcessBrokerClient,
    commands: PreparedExecution,
    loggers: Vec<BrokerLoggerId>,
    config: &ServiceConfig,
    signals: &mut TerminationSignals,
    mut controls: Option<&mut mpsc::Receiver<ControlCommand>>,
    service_name: Option<&str>,
) -> Result<SupervisionOutcome, ExecutorError> {
    let mut execution = ExecutionContext::new(config, loggers, controls.is_some());
    start_down_loggers(client, &mut execution).await?;

    loop {
        if execution.shutdown_requested && execution.lifecycle.is_none() {
            begin_descriptor_shutdown(client, &commands, config, &mut execution).await?;
            execution.shutdown_requested = false;
            continue;
        }
        if execution.machine.state() == SupervisorState::Initializing && execution.loggers_ready() {
            execution.machine.initialized()?;
            continue;
        }
        if fail_childless_start_on_logger_exhaustion(client, &mut execution).await? {
            continue;
        }
        if execution.auxiliary.is_idle()
            && execution.lifecycle.is_none()
            && (execution.loggers_ready()
                || matches!(
                    execution.machine.desired(),
                    DesiredState::Halt | DesiredState::Exit
                ))
            && advance_childless_state(
                &mut execution.machine,
                &mut execution.first_start,
                config.start_delay_seconds,
                &mut execution.deadline,
            )?
        {
            continue;
        }
        let lifecycle_finished = supervision_finished(
            &execution.machine,
            controls.is_some(),
            execution.first_start,
        );
        if advance_logger_shutdown(client, lifecycle_finished, &mut execution).await? {
            respond_abandoned_signals(&mut execution.pending_signals);
            return Ok(outcome(
                &execution.machine,
                &execution.tracker,
                &execution.status,
                execution.terminal_failure,
            ));
        }

        match next_executor_event(client, signals, &mut controls, execution.next_deadline()).await?
        {
            ExecutorEvent::Timer => {
                handle_due_logger_timers(client, &mut execution).await?;
                if execution
                    .deadline
                    .is_some_and(|deadline| deadline <= TokioInstant::now())
                {
                    handle_executor_timer(client, &commands, config, &mut execution).await?;
                }
            }
            ExecutorEvent::Shutdown => {
                cancel_auxiliary(client, &mut execution).await?;
                if config.process_mode == ProcessMode::DescriptorTracking
                    && execution.machine.state().live_generation().is_some()
                {
                    if execution.lifecycle.is_some() {
                        execution.shutdown_requested = true;
                    } else {
                        begin_descriptor_shutdown(client, &commands, config, &mut execution)
                            .await?;
                    }
                } else {
                    begin_supervisor_shutdown(
                        client,
                        &mut execution.machine,
                        &mut execution.pending_stop,
                        &mut execution.deadline,
                        &mut execution.stop_kill_sent,
                        &mut execution.pending_signals,
                    )
                    .await?;
                }
            }
            ExecutorEvent::Control(command) => {
                let name = service_name.ok_or(ExecutorError::ControlServerStopped)?;
                apply_control_command(client, name, command, &commands, config, &mut execution)
                    .await?;
            }
            ExecutorEvent::ControlClosed => return Err(ExecutorError::ControlServerStopped),
            ExecutorEvent::Broker(event) => {
                handle_broker_event(client, event, &commands, config, &mut execution).await?;
            }
        }
    }
}

async fn fail_childless_start_on_logger_exhaustion(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
) -> Result<bool, ExecutorError> {
    if !execution.loggers_failed() || execution.machine.state().live_generation().is_some() {
        return Ok(false);
    }
    match execution.machine.state() {
        SupervisorState::Initializing => execution.machine.initialized()?,
        SupervisorState::Down => {}
        SupervisorState::WaitingCondition | SupervisorState::Backoff { .. } => {
            if !execution.auxiliary.is_idle() {
                cancel_auxiliary(client, execution).await?;
                return Ok(true);
            }
            execution.machine.cancel_pending()?;
            execution.auxiliary = AuxiliaryExecution::Idle;
            execution.deadline = None;
        }
        SupervisorState::Starting(_)
        | SupervisorState::Running(_)
        | SupervisorState::Ready(_)
        | SupervisorState::Paused { .. }
        | SupervisorState::Stopping(_)
        | SupervisorState::Completed(_)
        | SupervisorState::Failed(_)
        | SupervisorState::Exited => return Ok(false),
    }
    execution
        .machine
        .fail_without_child(FailureReason::LoggerRetryLimit)?;
    Ok(true)
}

async fn advance_logger_shutdown(
    client: &mut ProcessBrokerClient,
    lifecycle_finished: bool,
    execution: &mut ExecutionContext,
) -> Result<bool, ExecutorError> {
    if !lifecycle_finished {
        return Ok(false);
    }
    if execution.loggers.is_empty() {
        execution.logger_shutdown = LoggerShutdownState::Complete;
        return Ok(true);
    }
    if execution.logger_shutdown == LoggerShutdownState::Running {
        execution.logger_shutdown = LoggerShutdownState::Preparing {
            deadline: TokioInstant::now() + LOGGER_PREPARE_GRACE,
        };
        for logger in &mut execution.loggers {
            if matches!(
                logger.state,
                LoggerExecutionState::Down
                    | LoggerExecutionState::Backoff { .. }
                    | LoggerExecutionState::Failed
            ) {
                logger.state = LoggerExecutionState::Down;
            }
        }
    }
    if matches!(
        execution.logger_shutdown,
        LoggerShutdownState::Preparing { .. }
    ) && execution.loggers.iter().all(|logger| {
        matches!(
            logger.state,
            LoggerExecutionState::Running { .. } | LoggerExecutionState::Down
        )
    }) {
        client.close_logger_inputs().await?;
        execution.logger_shutdown = LoggerShutdownState::ClosingInputs {
            deadline: TokioInstant::now() + BROKER_EVENT_TIMEOUT,
        };
    }
    advance_logger_shutdown_tier(execution);
    Ok(execution.logger_shutdown == LoggerShutdownState::Complete)
}

fn advance_logger_shutdown_tier(execution: &mut ExecutionContext) {
    let tier = match execution.logger_shutdown {
        LoggerShutdownState::Draining { tier, .. }
        | LoggerShutdownState::Terminating { tier, .. } => tier,
        LoggerShutdownState::Running
        | LoggerShutdownState::Preparing { .. }
        | LoggerShutdownState::ClosingInputs { .. }
        | LoggerShutdownState::Complete => return,
    };
    match next_logger_shutdown_tier(&execution.loggers, tier) {
        Some(next) if next == tier => {}
        Some(next) => {
            execution.logger_shutdown = LoggerShutdownState::Draining {
                deadline: TokioInstant::now() + LOGGER_DRAIN_GRACE,
                tier: next,
            };
        }
        None => execution.logger_shutdown = LoggerShutdownState::Complete,
    }
}

fn next_logger_shutdown_tier(
    loggers: &[LoggerExecution],
    tier: LoggerShutdownTier,
) -> Option<LoggerShutdownTier> {
    if !logger_tier_is_down(loggers, tier) {
        return Some(tier);
    }
    if tier == LoggerShutdownTier::FileAdapters
        && !logger_tier_is_down(loggers, LoggerShutdownTier::SharedLogger)
    {
        return Some(LoggerShutdownTier::SharedLogger);
    }
    None
}

fn logger_tier_is_down(loggers: &[LoggerExecution], tier: LoggerShutdownTier) -> bool {
    loggers
        .iter()
        .filter(|logger| logger_shutdown_tier(logger.logger) == tier)
        .all(|logger| matches!(logger.state, LoggerExecutionState::Down))
}

fn logger_shutdown_tier(logger: BrokerLoggerId) -> LoggerShutdownTier {
    if logger.is_shared_logger() {
        LoggerShutdownTier::SharedLogger
    } else {
        LoggerShutdownTier::FileAdapters
    }
}

fn advance_childless_state(
    machine: &mut StateMachine,
    first_start: &mut bool,
    start_delay_seconds: u64,
    deadline: &mut Option<TokioInstant>,
) -> Result<bool, TransitionError> {
    if machine.state() == SupervisorState::Down
        && matches!(machine.desired(), DesiredState::Up | DesiredState::Once)
    {
        machine.begin_start()?;
        let delay = if *first_start {
            Duration::from_secs(start_delay_seconds)
        } else {
            Duration::ZERO
        };
        *first_start = false;
        *deadline = Some(TokioInstant::now() + delay);
        return Ok(true);
    }
    if matches!(
        machine.state(),
        SupervisorState::Down | SupervisorState::Failed(_)
    ) && matches!(machine.desired(), DesiredState::Halt | DesiredState::Exit)
    {
        machine.exit_without_child()?;
        return Ok(true);
    }
    Ok(false)
}

fn supervision_finished(machine: &StateMachine, controlled: bool, first_start: bool) -> bool {
    machine.state() == SupervisorState::Exited
        || (!controlled
            && (matches!(machine.state(), SupervisorState::Failed(_))
                || (!first_start && machine.state() == SupervisorState::Down)))
}

enum ExecutorEvent {
    Broker(ProcessBrokerEvent),
    Shutdown,
    Control(ControlCommand),
    ControlClosed,
    Timer,
}

enum PendingSignal {
    Lifecycle,
    Control {
        command: ControlCommand,
        success: Box<Response>,
    },
}

struct ExecutionContext {
    auxiliary: AuxiliaryExecution,
    condition_tracker: ConditionTracker,
    deadline: Option<TokioInstant>,
    descriptor: Option<DescriptorExecution>,
    epoch: Instant,
    first_start: bool,
    logger_shutdown: LoggerShutdownState,
    lifecycle: Option<LifecycleExecution>,
    loggers: Vec<LoggerExecution>,
    machine: StateMachine,
    next_task: u64,
    pending_completion: Option<PendingCompletion>,
    pending_detach: Option<PendingDetach>,
    pending_signals: VecDeque<PendingSignal>,
    pending_stop: Option<StopCompletion>,
    status: RuntimeStatus,
    stop_kill_sent: bool,
    shutdown_requested: bool,
    tracker: RestartTracker,
    terminal_failure: Option<FailureReason>,
}

impl ExecutionContext {
    fn new(config: &ServiceConfig, loggers: Vec<BrokerLoggerId>, initializing: bool) -> Self {
        Self {
            auxiliary: AuxiliaryExecution::Idle,
            condition_tracker: ConditionTracker::default(),
            deadline: None,
            descriptor: None,
            epoch: Instant::now(),
            first_start: true,
            logger_shutdown: LoggerShutdownState::Running,
            lifecycle: None,
            loggers: loggers.into_iter().map(LoggerExecution::new).collect(),
            machine: if initializing {
                StateMachine::initializing(DesiredState::Up)
            } else {
                StateMachine::default()
            },
            next_task: 1,
            pending_completion: None,
            pending_detach: None,
            pending_signals: VecDeque::new(),
            pending_stop: None,
            status: RuntimeStatus::new(config),
            stop_kill_sent: false,
            shutdown_requested: false,
            tracker: RestartTracker::default(),
            terminal_failure: None,
        }
    }

    fn loggers_ready(&self) -> bool {
        self.loggers
            .iter()
            .all(|logger| matches!(logger.state, LoggerExecutionState::Running { .. }))
    }

    fn loggers_failed(&self) -> bool {
        self.loggers
            .iter()
            .any(|logger| matches!(logger.state, LoggerExecutionState::Failed))
    }

    fn next_deadline(&self) -> Option<TokioInstant> {
        self.loggers
            .iter()
            .filter_map(|logger| logger.state.deadline())
            .fold(
                self.logger_shutdown.deadline().or(self.deadline),
                |current, deadline| Some(current.map_or(deadline, |current| current.min(deadline))),
            )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleHookKind {
    Reload,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifecycleHookState {
    Starting,
    Running,
    Killing,
    WaitingLifetime,
}

struct LifecycleExecution {
    after: Option<StopCompletion>,
    command: Option<ControlCommand>,
    generation: Generation,
    kind: LifecycleHookKind,
    previous_desired: DesiredState,
    response: Option<Response>,
    resume_ready: bool,
    state: LifecycleHookState,
    task: BrokerTaskId,
    timeout: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DescriptorExecution {
    generation: Generation,
    launcher_result: Option<ChildResult>,
    lifetime_preceded_launcher: bool,
    lifetime_result: Option<ChildResult>,
}

impl DescriptorExecution {
    const fn new(generation: Generation) -> Self {
        Self {
            generation,
            launcher_result: None,
            lifetime_preceded_launcher: false,
            lifetime_result: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoggerShutdownTier {
    FileAdapters,
    SharedLogger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LoggerShutdownState {
    Running,
    Preparing {
        deadline: TokioInstant,
    },
    ClosingInputs {
        deadline: TokioInstant,
    },
    Draining {
        deadline: TokioInstant,
        tier: LoggerShutdownTier,
    },
    Terminating {
        deadline: TokioInstant,
        kill_sent: bool,
        tier: LoggerShutdownTier,
    },
    Complete,
}

impl LoggerShutdownState {
    const fn deadline(self) -> Option<TokioInstant> {
        match self {
            Self::Preparing { deadline }
            | Self::ClosingInputs { deadline }
            | Self::Draining { deadline, .. }
            | Self::Terminating { deadline, .. } => Some(deadline),
            Self::Running | Self::Complete => None,
        }
    }

    const fn permits_restart(self) -> bool {
        matches!(self, Self::Running | Self::Preparing { .. })
    }
}

struct LoggerExecution {
    failure_streak: u32,
    logger: BrokerLoggerId,
    state: LoggerExecutionState,
}

impl LoggerExecution {
    const fn new(logger: BrokerLoggerId) -> Self {
        Self {
            failure_streak: 0,
            logger,
            state: LoggerExecutionState::Down,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum LoggerExecutionState {
    Down,
    Starting {
        task: BrokerTaskId,
        deadline: TokioInstant,
    },
    Running {
        task: BrokerTaskId,
        started_at: Instant,
    },
    Backoff {
        deadline: TokioInstant,
    },
    Failed,
    Killing {
        task: BrokerTaskId,
        deadline: TokioInstant,
    },
}

impl LoggerExecutionState {
    const fn task(self) -> Option<BrokerTaskId> {
        match self {
            Self::Starting { task, .. }
            | Self::Running { task, .. }
            | Self::Killing { task, .. } => Some(task),
            Self::Down | Self::Backoff { .. } | Self::Failed => None,
        }
    }

    const fn deadline(self) -> Option<TokioInstant> {
        match self {
            Self::Starting { deadline, .. }
            | Self::Backoff { deadline }
            | Self::Killing { deadline, .. } => Some(deadline),
            Self::Down | Self::Running { .. } | Self::Failed => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
enum AuxiliaryExecution {
    #[default]
    Idle,
    Starting(BrokerTaskId),
    Running(BrokerTaskId),
    Killing {
        task: BrokerTaskId,
        retry: bool,
    },
    Passed,
    HookStarting(BrokerTaskId),
    HookRunning(BrokerTaskId),
    HookKilling(BrokerTaskId),
}

impl AuxiliaryExecution {
    const fn is_idle(self) -> bool {
        matches!(self, Self::Idle | Self::Passed)
    }

    const fn is_hook(self) -> bool {
        matches!(
            self,
            Self::HookStarting(_) | Self::HookRunning(_) | Self::HookKilling(_)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingCompletion {
    generation: Generation,
    policy_result: ChildResult,
    runtime_seconds: u64,
    stop: Option<StopCompletion>,
}

async fn handle_executor_timer(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if execution.lifecycle.is_some() {
        return handle_lifecycle_timer(client, commands, config, execution).await;
    }
    match execution.machine.state() {
        SupervisorState::WaitingCondition => {
            handle_waiting_timer(client, commands, config, execution).await?;
        }
        SupervisorState::Backoff { generation, .. } => {
            execution.deadline = None;
            execution.machine.backoff_elapsed(generation)?;
        }
        SupervisorState::Stopping(generation) if !execution.stop_kill_sent => {
            client
                .signal(generation, BrokerSignalScope::Group, ProcessSignal::Kill)
                .await?;
            execution
                .pending_signals
                .push_back(PendingSignal::Lifecycle);
            execution.stop_kill_sent = true;
            execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
        }
        SupervisorState::Stopping(_) => {
            return Err(ExecutorError::BrokerTimedOut("service termination"));
        }
        SupervisorState::Starting(_) => {
            return Err(ExecutorError::BrokerTimedOut("service startup"));
        }
        SupervisorState::Running(_) => {
            return Err(ExecutorError::BrokerTimedOut("service readiness"));
        }
        SupervisorState::Completed(_) => match execution.auxiliary {
            AuxiliaryExecution::HookRunning(task) => {
                client
                    .signal_task(task, BrokerSignalScope::Group, ProcessSignal::Kill)
                    .await?;
                execution.auxiliary = AuxiliaryExecution::HookKilling(task);
                execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            }
            AuxiliaryExecution::HookStarting(_) => {
                return Err(ExecutorError::BrokerTimedOut("post-exit hook startup"));
            }
            AuxiliaryExecution::HookKilling(_) => {
                return Err(ExecutorError::BrokerTimedOut("post-exit hook cleanup"));
            }
            _ => {
                return Err(ExecutorError::Transition(TransitionError::Invalid {
                    state: execution.machine.state(),
                    event: "post_exit_timer",
                }));
            }
        },
        state => {
            return Err(ExecutorError::Transition(TransitionError::Invalid {
                state,
                event: "executor_timer",
            }));
        }
    }
    Ok(())
}

async fn handle_lifecycle_timer(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let Some(lifecycle) = execution.lifecycle.as_mut() else {
        return Ok(());
    };
    match lifecycle.state {
        LifecycleHookState::Running => {
            let task = lifecycle.task;
            lifecycle.state = LifecycleHookState::Killing;
            execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            client
                .signal_task(task, BrokerSignalScope::Group, ProcessSignal::Kill)
                .await
                .map_err(Into::into)
        }
        LifecycleHookState::WaitingLifetime => {
            finish_lifecycle_failure(
                client,
                commands,
                config,
                execution,
                "descriptor lifetime did not close before its configured deadline",
            )
            .await
        }
        LifecycleHookState::Starting => Err(ExecutorError::BrokerTimedOut(
            "descriptor lifecycle hook startup",
        )),
        LifecycleHookState::Killing => Err(ExecutorError::BrokerTimedOut(
            "descriptor lifecycle hook cleanup",
        )),
    }
}

async fn handle_waiting_timer(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let condition = config.start_condition.as_ref();
    let condition_command = commands.condition.as_ref();
    match (condition, condition_command, execution.auxiliary) {
        (None, None, _) | (Some(_), Some(_), AuxiliaryExecution::Passed) => {
            start_service(client, commands, config, execution).await
        }
        (Some(_), Some(command), AuxiliaryExecution::Idle) => {
            let task = allocate_task(execution)?;
            client
                .spawn_task(task, command.clone(), CHILD_STARTUP_TIMEOUT)
                .await?;
            execution.auxiliary = AuxiliaryExecution::Starting(task);
            execution.deadline =
                Some(TokioInstant::now() + CHILD_STARTUP_TIMEOUT + BROKER_EVENT_TIMEOUT);
            Ok(())
        }
        (Some(_), Some(_), AuxiliaryExecution::Running(task)) => {
            client
                .signal_task(task, BrokerSignalScope::Group, ProcessSignal::Kill)
                .await?;
            execution.auxiliary = AuxiliaryExecution::Killing { task, retry: true };
            execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            Ok(())
        }
        (Some(_), Some(_), AuxiliaryExecution::Starting(_)) => {
            Err(ExecutorError::BrokerTimedOut("condition startup"))
        }
        (Some(_), Some(_), AuxiliaryExecution::Killing { .. }) => {
            Err(ExecutorError::BrokerTimedOut("condition cleanup"))
        }
        _ => Err(ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared condition does not match validated configuration",
        ))),
    }
}

async fn cancel_auxiliary(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let (task, hook) = match execution.auxiliary {
        AuxiliaryExecution::Starting(task) | AuxiliaryExecution::Running(task) => (task, false),
        AuxiliaryExecution::HookStarting(task) | AuxiliaryExecution::HookRunning(task) => {
            (task, true)
        }
        AuxiliaryExecution::Idle
        | AuxiliaryExecution::Killing { .. }
        | AuxiliaryExecution::Passed
        | AuxiliaryExecution::HookKilling(_) => return Ok(()),
    };
    client
        .signal_task(task, BrokerSignalScope::Group, ProcessSignal::Kill)
        .await?;
    execution.auxiliary = if hook {
        AuxiliaryExecution::HookKilling(task)
    } else {
        AuxiliaryExecution::Killing { task, retry: false }
    };
    execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
    Ok(())
}

fn allocate_task(execution: &mut ExecutionContext) -> io::Result<BrokerTaskId> {
    let task = BrokerTaskId::new(execution.next_task).ok_or_else(|| {
        io::Error::other("pre-start condition task identifier space is exhausted")
    })?;
    execution.next_task = execution
        .next_task
        .checked_add(1)
        .ok_or_else(|| io::Error::other("pre-start condition task identifier overflow"))?;
    Ok(task)
}

async fn start_down_loggers(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if !execution.logger_shutdown.permits_restart() {
        return Ok(());
    }
    loop {
        let Some(position) = execution
            .loggers
            .iter()
            .position(|logger| matches!(logger.state, LoggerExecutionState::Down))
        else {
            return Ok(());
        };
        let logger = execution
            .loggers
            .get(position)
            .ok_or_else(|| io::Error::other("selected logger disappeared"))?
            .logger;
        let task = allocate_task(execution)?;
        client
            .spawn_logger(task, logger, CHILD_STARTUP_TIMEOUT)
            .await?;
        let state = execution
            .loggers
            .get_mut(position)
            .ok_or_else(|| io::Error::other("selected logger disappeared"))?;
        state.state = LoggerExecutionState::Starting {
            task,
            deadline: TokioInstant::now() + CHILD_STARTUP_TIMEOUT + BROKER_EVENT_TIMEOUT,
        };
    }
}

async fn handle_due_logger_timers(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let now = TokioInstant::now();
    match execution.logger_shutdown {
        LoggerShutdownState::Preparing { deadline } if deadline <= now => {
            client.close_logger_inputs().await?;
            execution.logger_shutdown = LoggerShutdownState::ClosingInputs {
                deadline: now + BROKER_EVENT_TIMEOUT,
            };
            return Ok(());
        }
        LoggerShutdownState::ClosingInputs { deadline } if deadline <= now => {
            return Err(ExecutorError::BrokerTimedOut("closing logger inputs"));
        }
        LoggerShutdownState::Draining { deadline, tier } if deadline <= now => {
            signal_logger_tier(client, execution, tier, ProcessSignal::Terminate).await?;
            execution.logger_shutdown = LoggerShutdownState::Terminating {
                deadline: now + LOGGER_STOP_GRACE,
                kill_sent: false,
                tier,
            };
            return Ok(());
        }
        LoggerShutdownState::Terminating {
            deadline,
            kill_sent: false,
            tier,
        } if deadline <= now => {
            signal_logger_tier(client, execution, tier, ProcessSignal::Kill).await?;
            execution.logger_shutdown = LoggerShutdownState::Terminating {
                deadline: now + BROKER_EVENT_TIMEOUT,
                kill_sent: true,
                tier,
            };
            return Ok(());
        }
        LoggerShutdownState::Terminating {
            deadline,
            kill_sent: true,
            ..
        } if deadline <= now => {
            return Err(ExecutorError::BrokerTimedOut("logger termination"));
        }
        LoggerShutdownState::Running
        | LoggerShutdownState::Preparing { .. }
        | LoggerShutdownState::ClosingInputs { .. }
        | LoggerShutdownState::Draining { .. }
        | LoggerShutdownState::Terminating { .. }
        | LoggerShutdownState::Complete => {}
    }
    if !execution.logger_shutdown.permits_restart() {
        return Ok(());
    }
    for logger in &mut execution.loggers {
        match logger.state {
            LoggerExecutionState::Backoff { deadline } if deadline <= now => {
                logger.state = LoggerExecutionState::Down;
            }
            LoggerExecutionState::Starting { task, deadline } if deadline <= now => {
                client
                    .signal_task(task, BrokerSignalScope::Group, ProcessSignal::Kill)
                    .await?;
                logger.state = LoggerExecutionState::Killing {
                    task,
                    deadline: now + BROKER_EVENT_TIMEOUT,
                };
            }
            LoggerExecutionState::Killing { deadline, .. } if deadline <= now => {
                return Err(ExecutorError::BrokerTimedOut("logger cleanup"));
            }
            LoggerExecutionState::Down
            | LoggerExecutionState::Starting { .. }
            | LoggerExecutionState::Running { .. }
            | LoggerExecutionState::Backoff { .. }
            | LoggerExecutionState::Failed
            | LoggerExecutionState::Killing { .. } => {}
        }
    }
    start_down_loggers(client, execution).await
}

async fn signal_logger_tier(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
    tier: LoggerShutdownTier,
    signal: ProcessSignal,
) -> Result<(), ExecutorError> {
    let deadline = TokioInstant::now() + BROKER_EVENT_TIMEOUT;
    for logger in execution
        .loggers
        .iter_mut()
        .filter(|logger| logger_shutdown_tier(logger.logger) == tier)
    {
        let Some(task) = logger.state.task() else {
            logger.state = LoggerExecutionState::Down;
            continue;
        };
        client
            .signal_task(task, BrokerSignalScope::Group, signal)
            .await?;
        if signal == ProcessSignal::Terminate {
            client
                .signal_task(task, BrokerSignalScope::Group, ProcessSignal::Continue)
                .await?;
        }
        logger.state = LoggerExecutionState::Killing { task, deadline };
    }
    Ok(())
}

fn schedule_logger_restart(
    logger: &mut LoggerExecution,
    broker: ProcessId,
    restart: &LoggerRestartConfig,
) {
    let runtime = match logger.state {
        LoggerExecutionState::Running { started_at, .. } => Some(started_at.elapsed()),
        LoggerExecutionState::Down
        | LoggerExecutionState::Starting { .. }
        | LoggerExecutionState::Backoff { .. }
        | LoggerExecutionState::Failed
        | LoggerExecutionState::Killing { .. } => None,
    };
    if runtime
        .is_some_and(|runtime| runtime >= Duration::from_secs(restart.backoff.reset_after_seconds))
    {
        logger.failure_streak = 0;
    }
    let Some(failure_streak) = logger.failure_streak.checked_add(1) else {
        logger.state = LoggerExecutionState::Failed;
        return;
    };
    logger.failure_streak = failure_streak;
    if restart
        .max_retries
        .is_some_and(|max_retries| failure_streak > max_retries)
    {
        logger.state = LoggerExecutionState::Failed;
        return;
    }
    let exponent = logger.failure_streak.saturating_sub(1);
    let factor = u64::from(restart.backoff.multiplier).saturating_pow(exponent);
    let base_seconds = restart
        .backoff
        .initial_seconds
        .saturating_mul(factor)
        .min(restart.backoff.max_seconds);
    let seed = u64::from(logger.logger.pipeline())
        .saturating_mul(u64::from(u16::MAX).saturating_add(1))
        .saturating_add(u64::from(logger.logger.stage()));
    logger.state = LoggerExecutionState::Backoff {
        deadline: TokioInstant::now()
            + jittered_backoff_seed(base_seconds, restart.backoff.jitter_percent, seed, broker),
    };
}

async fn start_service(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let generation = execution.machine.preconditions_ready()?;
    execution.auxiliary = AuxiliaryExecution::Idle;
    execution
        .tracker
        .record_start(elapsed_seconds(execution.epoch));
    execution.status.started_at = Some(Instant::now());
    execution.status.down_since = None;
    execution.status.readiness_failed = false;
    if config.process_mode == ProcessMode::DescriptorTracking {
        execution.descriptor = Some(DescriptorExecution::new(generation));
        let readiness_timeout = (config.readiness.mode == ReadinessMode::NotifyFd)
            .then(|| Duration::from_secs(config.readiness.timeout_seconds));
        client
            .spawn_with_lifetime(
                generation,
                commands.service.clone(),
                CHILD_STARTUP_TIMEOUT,
                readiness_timeout,
            )
            .await?;
    } else if config.readiness.mode == ReadinessMode::Immediate {
        client
            .spawn(generation, commands.service.clone(), CHILD_STARTUP_TIMEOUT)
            .await?;
    } else {
        client
            .spawn_with_readiness(
                generation,
                commands.service.clone(),
                CHILD_STARTUP_TIMEOUT,
                Duration::from_secs(config.readiness.timeout_seconds),
            )
            .await?;
    }
    execution.deadline = Some(TokioInstant::now() + CHILD_STARTUP_TIMEOUT + BROKER_EVENT_TIMEOUT);
    Ok(())
}

struct PendingDetach {
    generation: Generation,
    previous_desired: DesiredState,
    command: ControlCommand,
    success: Response,
}

struct RuntimeStatus {
    command: Vec<String>,
    down_since: Option<Instant>,
    failures: u64,
    last_result: Option<ChildResult>,
    last_readiness_failed: bool,
    last_start_failed: bool,
    main_pid: Option<u32>,
    main_pid_file: Option<OwnedPidFile>,
    main_pid_path: Option<std::path::PathBuf>,
    readiness_failed: bool,
    started_at: Option<Instant>,
}

impl RuntimeStatus {
    fn new(config: &ServiceConfig) -> Self {
        Self {
            command: config.command.clone(),
            down_since: None,
            failures: 0,
            last_result: None,
            last_readiness_failed: false,
            last_start_failed: false,
            main_pid: None,
            main_pid_file: None,
            main_pid_path: config.pid_files.main.clone(),
            readiness_failed: false,
            started_at: None,
        }
    }

    fn publish_main_pid(&mut self, process: ProcessId) -> io::Result<()> {
        let process = u32::try_from(process.get())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "child PID is outside u32"))?;
        self.main_pid_file = self
            .main_pid_path
            .as_deref()
            .map(|path| OwnedPidFile::publish(path, process))
            .transpose()?;
        self.main_pid = Some(process);
        Ok(())
    }

    fn clear_main_pid(&mut self) {
        self.main_pid = None;
        self.main_pid_file = None;
    }

    fn snapshot(
        &self,
        machine: &StateMachine,
        tracker: &RestartTracker,
        deadline: Option<TokioInstant>,
        loggers: &[LoggerExecution],
    ) -> StatusSnapshot {
        let mut snapshot = StatusSnapshot::from_machine(machine);
        snapshot.supervisor_pid = Some(std::process::id());
        snapshot.main_pid = self.main_pid;
        snapshot.uptime_seconds = self.started_at.map(elapsed_seconds);
        snapshot.down_seconds = self.down_since.map(elapsed_seconds);
        snapshot.starts = tracker.total_starts();
        snapshot.failures = self.failures;
        snapshot.last_result = self.last_result.map(|result| {
            if self.last_readiness_failed {
                LastResult::ReadinessTimeout
            } else if self.last_start_failed {
                LastResult::SpawnFailed
            } else {
                match result {
                    ChildResult::Exited(code) => LastResult::Exited(code),
                    ChildResult::Signaled(signal) => LastResult::Signaled(signal),
                    ChildResult::LifetimeClosed => LastResult::LifetimeClosed,
                    ChildResult::LifetimeFailed => LastResult::LifetimeFailed,
                }
            }
        });
        snapshot.backoff_seconds = if matches!(machine.state(), SupervisorState::Backoff { .. }) {
            deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(TokioInstant::now())
                    .as_secs()
            })
        } else {
            None
        };
        snapshot.logger = logger_status(loggers);
        snapshot.command.clone_from(&self.command);
        snapshot
    }
}

fn logger_status(loggers: &[LoggerExecution]) -> LoggerStatus {
    if loggers.is_empty() {
        LoggerStatus::NotConfigured
    } else if loggers
        .iter()
        .any(|logger| matches!(logger.state, LoggerExecutionState::Failed))
    {
        LoggerStatus::Failed
    } else if loggers
        .iter()
        .any(|logger| matches!(logger.state, LoggerExecutionState::Backoff { .. }))
    {
        LoggerStatus::Backoff
    } else if loggers
        .iter()
        .all(|logger| matches!(logger.state, LoggerExecutionState::Running { .. }))
    {
        LoggerStatus::Ready
    } else {
        LoggerStatus::Starting
    }
}

async fn next_executor_event(
    client: &mut ProcessBrokerClient,
    signals: &mut TerminationSignals,
    controls: &mut Option<&mut mpsc::Receiver<ControlCommand>>,
    deadline: Option<TokioInstant>,
) -> Result<ExecutorEvent, ExecutorError> {
    tokio::select! {
        event = client.next_event() => Ok(ExecutorEvent::Broker(event?)),
        shutdown = signals.recv() => {
            shutdown?;
            Ok(ExecutorEvent::Shutdown)
        }
        command = receive_control(controls) => Ok(command.map_or(
            ExecutorEvent::ControlClosed,
            ExecutorEvent::Control,
        )),
        () = wait_for_deadline(deadline) => Ok(ExecutorEvent::Timer),
    }
}

async fn receive_control(
    controls: &mut Option<&mut mpsc::Receiver<ControlCommand>>,
) -> Option<ControlCommand> {
    match controls {
        Some(receiver) => receiver.recv().await,
        None => pending().await,
    }
}

async fn wait_for_deadline(deadline: Option<TokioInstant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

async fn begin_supervisor_shutdown(
    client: &mut ProcessBrokerClient,
    machine: &mut StateMachine,
    pending_stop: &mut Option<StopCompletion>,
    deadline: &mut Option<TokioInstant>,
    stop_kill_sent: &mut bool,
    pending_signals: &mut VecDeque<PendingSignal>,
) -> Result<(), ExecutorError> {
    machine.set_desired(DesiredState::Halt);
    match machine.state() {
        SupervisorState::WaitingCondition | SupervisorState::Backoff { .. } => {
            machine.cancel_pending()?;
            *deadline = None;
        }
        SupervisorState::Starting(generation)
        | SupervisorState::Running(generation)
        | SupervisorState::Ready(generation)
        | SupervisorState::Paused { generation, .. } => {
            machine.begin_stop(generation)?;
            begin_group_stop(
                client,
                generation,
                pending_stop,
                StopCompletion::Halt,
                deadline,
                stop_kill_sent,
                pending_signals,
            )
            .await?;
        }
        SupervisorState::Stopping(_) => {
            *pending_stop = Some(StopCompletion::Halt);
        }
        SupervisorState::Down
        | SupervisorState::Completed(_)
        | SupervisorState::Failed(_)
        | SupervisorState::Exited => {}
        SupervisorState::Initializing => {
            return Err(ExecutorError::Transition(TransitionError::Invalid {
                state: SupervisorState::Initializing,
                event: "supervisor_shutdown",
            }));
        }
    }
    Ok(())
}

async fn apply_control_command(
    client: &mut ProcessBrokerClient,
    service_name: &str,
    command: ControlCommand,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if let Some(response) = control_preflight_rejection(&command, execution) {
        let _ = command.respond(response);
        return Ok(());
    }

    let previous_desired = execution.machine.desired();
    let operation = command.request().operation;
    let decision = decide_request(service_name, &mut execution.machine, command.request());
    let mut response = decision.response;
    if operation == Operation::Status && response.code == ResponseCode::Ok {
        response.status = Some(execution.status.snapshot(
            &execution.machine,
            &execution.tracker,
            execution.deadline,
            &execution.loggers,
        ));
    }
    if response.code == ResponseCode::Ok
        && matches!(
            operation,
            Operation::Start | Operation::Once | Operation::Restart
        )
        && reset_failed_loggers(execution)
    {
        start_down_loggers(client, execution).await?;
    }
    let accepted = AcceptedControl {
        command,
        effect: decision.effect,
        previous_desired,
        response,
    };
    let Some(accepted) =
        apply_descriptor_control(client, accepted, commands, config, execution).await?
    else {
        return Ok(());
    };
    apply_standard_control(client, accepted, execution).await
}

async fn apply_standard_control(
    client: &mut ProcessBrokerClient,
    accepted: AcceptedControl,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let AcceptedControl {
        command,
        effect,
        previous_desired,
        response,
    } = accepted;
    match effect {
        ControlEffect::None | ControlEffect::BeginStart => {}
        ControlEffect::CancelPending { .. } => {
            cancel_auxiliary(client, execution).await?;
            execution.machine.cancel_pending()?;
            if execution.auxiliary.is_idle() {
                execution.deadline = None;
            }
        }
        ControlEffect::StopGroup { generation, after } => {
            if matches!(execution.machine.state(), SupervisorState::Stopping(_)) {
                execution.pending_stop = Some(after);
            } else {
                execution.machine.begin_stop(generation)?;
                begin_group_stop(
                    client,
                    generation,
                    &mut execution.pending_stop,
                    after,
                    &mut execution.deadline,
                    &mut execution.stop_kill_sent,
                    &mut execution.pending_signals,
                )
                .await?;
            }
        }
        ControlEffect::ExitSupervisor { leave_child: false } => {
            cancel_auxiliary(client, execution).await?;
            if matches!(
                execution.machine.state(),
                SupervisorState::WaitingCondition | SupervisorState::Backoff { .. }
            ) {
                execution.machine.cancel_pending()?;
                if execution.auxiliary.is_idle() {
                    execution.deadline = None;
                }
            }
        }
        ControlEffect::ExitSupervisor { leave_child: true } => {
            let Some(generation) = execution.machine.state().live_generation() else {
                return Err(ExecutorError::Transition(TransitionError::Invalid {
                    state: execution.machine.state(),
                    event: "begin_detach",
                }));
            };
            execution.machine.set_desired(previous_desired);
            client.detach(generation).await?;
            execution.pending_detach = Some(PendingDetach {
                generation,
                previous_desired,
                command,
                success: response,
            });
            return Ok(());
        }
        ControlEffect::DeliverSignal {
            generation,
            scope,
            signal,
        } => {
            client
                .signal(generation, broker_scope(scope), process_signal(signal))
                .await?;
            execution.pending_signals.push_back(PendingSignal::Control {
                command,
                success: Box::new(response),
            });
            return Ok(());
        }
    }
    let _ = command.respond(response);
    Ok(())
}

struct AcceptedControl {
    command: ControlCommand,
    effect: ControlEffect,
    previous_desired: DesiredState,
    response: Response,
}

async fn apply_descriptor_control(
    client: &mut ProcessBrokerClient,
    accepted: AcceptedControl,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<Option<AcceptedControl>, ExecutorError> {
    if config.process_mode != ProcessMode::DescriptorTracking {
        return Ok(Some(accepted));
    }
    let AcceptedControl {
        command,
        effect,
        previous_desired,
        mut response,
    } = accepted;
    let request = match effect {
        ControlEffect::StopGroup { generation, after } => {
            let resume_ready = generation_is_ready(execution.machine.state(), generation);
            execution.machine.begin_stop(generation)?;
            LifecycleHookRequest {
                after: Some(after),
                command: Some(command),
                generation,
                kind: LifecycleHookKind::Stop,
                previous_desired,
                response: Some(response),
                resume_ready,
            }
        }
        ControlEffect::DeliverSignal {
            generation,
            signal: Signal::Hangup,
            ..
        } => LifecycleHookRequest {
            after: None,
            command: Some(command),
            generation,
            kind: LifecycleHookKind::Reload,
            previous_desired,
            response: Some(response),
            resume_ready: true,
        },
        ControlEffect::ExitSupervisor { leave_child: true } => {
            execution.machine.set_desired(previous_desired);
            response.code = ResponseCode::Invalid;
            "descriptor-tracked services cannot be detached from their supervisor"
                .clone_into(&mut response.message);
            let _ = command.respond(response);
            return Ok(None);
        }
        ControlEffect::DeliverSignal { .. } => {
            response.code = ResponseCode::Invalid;
            "descriptor-tracked services reject raw signals; use stop, restart, halt, or HUP reload"
                .clone_into(&mut response.message);
            let _ = command.respond(response);
            return Ok(None);
        }
        effect => {
            return Ok(Some(AcceptedControl {
                command,
                effect,
                previous_desired,
                response,
            }));
        }
    };
    let template = match request.kind {
        LifecycleHookKind::Reload => commands.descriptor_reload.as_ref(),
        LifecycleHookKind::Stop => commands.descriptor_stop.as_ref(),
    };
    begin_lifecycle_hook(client, template, config, execution, request).await?;
    Ok(None)
}

struct LifecycleHookRequest {
    after: Option<StopCompletion>,
    command: Option<ControlCommand>,
    generation: Generation,
    kind: LifecycleHookKind,
    previous_desired: DesiredState,
    response: Option<Response>,
    resume_ready: bool,
}

async fn begin_lifecycle_hook(
    client: &mut ProcessBrokerClient,
    template: Option<&ProcessCommand>,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
    request: LifecycleHookRequest,
) -> Result<(), ExecutorError> {
    if execution.lifecycle.is_some() {
        return Err(ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "a descriptor lifecycle hook is already running",
        )));
    }
    let tracking = config.descriptor_tracking.as_ref().ok_or_else(|| {
        ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "descriptor lifecycle configuration is absent",
        ))
    })?;
    let template = template.ok_or_else(|| {
        ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared descriptor lifecycle command is absent",
        ))
    })?;
    let timeout_seconds = match request.kind {
        LifecycleHookKind::Reload => tracking.reload.timeout_seconds,
        LifecycleHookKind::Stop => tracking.stop.timeout_seconds,
    };
    let task = allocate_task(execution)?;
    client
        .spawn_task(task, template.clone(), CHILD_STARTUP_TIMEOUT)
        .await?;
    execution.lifecycle = Some(LifecycleExecution {
        after: request.after,
        command: request.command,
        generation: request.generation,
        kind: request.kind,
        previous_desired: request.previous_desired,
        response: request.response,
        resume_ready: request.resume_ready,
        state: LifecycleHookState::Starting,
        task,
        timeout: Duration::from_secs(timeout_seconds),
    });
    execution.deadline = Some(TokioInstant::now() + CHILD_STARTUP_TIMEOUT + BROKER_EVENT_TIMEOUT);
    Ok(())
}

async fn begin_descriptor_shutdown(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if execution.lifecycle.is_some() {
        return Ok(());
    }
    let generation = execution.machine.state().live_generation().ok_or_else(|| {
        ExecutorError::Transition(TransitionError::Invalid {
            state: execution.machine.state(),
            event: "descriptor_shutdown",
        })
    })?;
    let previous_desired = execution.machine.desired();
    let resume_ready = generation_is_ready(execution.machine.state(), generation);
    execution.machine.set_desired(DesiredState::Halt);
    execution.machine.begin_stop(generation)?;
    begin_lifecycle_hook(
        client,
        commands.descriptor_stop.as_ref(),
        config,
        execution,
        LifecycleHookRequest {
            after: Some(StopCompletion::Halt),
            command: None,
            generation,
            kind: LifecycleHookKind::Stop,
            previous_desired,
            response: None,
            resume_ready,
        },
    )
    .await
}

fn generation_is_ready(state: SupervisorState, generation: Generation) -> bool {
    matches!(
        state,
        SupervisorState::Ready(current) if current == generation
    ) || matches!(
        state,
        SupervisorState::Paused {
            generation: current,
            ready: true,
        } if current == generation
    )
}

fn reset_failed_loggers(execution: &mut ExecutionContext) -> bool {
    let mut reset = false;
    for logger in &mut execution.loggers {
        if matches!(logger.state, LoggerExecutionState::Failed) {
            logger.failure_streak = 0;
            logger.state = LoggerExecutionState::Down;
            reset = true;
        }
    }
    reset
}

fn control_preflight_rejection(
    command: &ControlCommand,
    execution: &ExecutionContext,
) -> Option<Response> {
    let operation = command.request().operation;
    if operation == Operation::Exit && !execution.loggers.is_empty() {
        return Some(Response {
            code: ResponseCode::Invalid,
            generation: execution.machine.state().generation(),
            message: "exit cannot detach a service with broker-owned logging pipelines".to_owned(),
            status: None,
        });
    }
    if execution.pending_detach.is_some() && operation != Operation::Status {
        return Some(Response {
            code: ResponseCode::Conflict,
            generation: execution.machine.state().generation(),
            message: "a live-child detach operation is already pending".to_owned(),
            status: None,
        });
    }
    if execution.lifecycle.is_some() && operation != Operation::Status {
        return Some(Response {
            code: ResponseCode::Conflict,
            generation: execution.machine.state().generation(),
            message: "a descriptor lifecycle hook is already pending".to_owned(),
            status: None,
        });
    }
    None
}

async fn begin_group_stop(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    pending_stop: &mut Option<StopCompletion>,
    after: StopCompletion,
    deadline: &mut Option<TokioInstant>,
    stop_kill_sent: &mut bool,
    pending_signals: &mut VecDeque<PendingSignal>,
) -> Result<(), ExecutorError> {
    request_group_stop(client, generation, pending_signals).await?;
    *pending_stop = Some(after);
    *stop_kill_sent = false;
    *deadline = Some(TokioInstant::now() + SERVICE_STOP_GRACE);
    Ok(())
}

async fn request_group_stop(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    pending_signals: &mut VecDeque<PendingSignal>,
) -> Result<(), ExecutorError> {
    client
        .signal(
            generation,
            BrokerSignalScope::Group,
            ProcessSignal::Terminate,
        )
        .await?;
    pending_signals.push_back(PendingSignal::Lifecycle);
    client
        .signal(
            generation,
            BrokerSignalScope::Group,
            ProcessSignal::Continue,
        )
        .await?;
    pending_signals.push_back(PendingSignal::Lifecycle);
    Ok(())
}

async fn handle_broker_event(
    client: &mut ProcessBrokerClient,
    event: ProcessBrokerEvent,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if matches!(
        event,
        ProcessBrokerEvent::ContainmentFailed { .. }
            | ProcessBrokerEvent::TaskContainmentFailed { .. }
    ) {
        return Err(ExecutorError::ContainmentFailed(event));
    }
    if matches!(event, ProcessBrokerEvent::LoggerInputsClosed)
        && matches!(
            execution.logger_shutdown,
            LoggerShutdownState::ClosingInputs { .. }
        )
    {
        let tier = if logger_tier_is_down(&execution.loggers, LoggerShutdownTier::FileAdapters) {
            LoggerShutdownTier::SharedLogger
        } else {
            LoggerShutdownTier::FileAdapters
        };
        execution.logger_shutdown = LoggerShutdownState::Draining {
            deadline: TokioInstant::now() + LOGGER_DRAIN_GRACE,
            tier,
        };
        return Ok(());
    }
    if task_id(&event).is_some_and(|task| {
        execution
            .lifecycle
            .as_ref()
            .is_some_and(|lifecycle| lifecycle.task == task)
    }) {
        return handle_lifecycle_event(client, event, commands, config, execution).await;
    }
    if task_id(&event).is_some_and(|task| {
        execution
            .loggers
            .iter()
            .any(|logger| logger.state.task() == Some(task))
    }) {
        return handle_logger_event(client, event, config, execution);
    }
    if task_id(&event).is_some() {
        return handle_auxiliary_event(client, event, config, execution);
    }
    handle_service_broker_event(client, event, commands, config, execution).await
}

async fn handle_lifecycle_event(
    client: &mut ProcessBrokerClient,
    event: ProcessBrokerEvent,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let (task, state) = execution
        .lifecycle
        .as_ref()
        .map(|lifecycle| (lifecycle.task, lifecycle.state))
        .ok_or_else(|| ExecutorError::UnexpectedBrokerEvent(event.clone()))?;
    match event {
        ProcessBrokerEvent::TaskStarted { task: current, .. }
            if current == task && state == LifecycleHookState::Starting =>
        {
            let lifecycle = execution.lifecycle.as_mut().ok_or_else(|| {
                ExecutorError::OperatingSystem(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "started descriptor lifecycle hook is absent",
                ))
            })?;
            lifecycle.state = LifecycleHookState::Running;
            execution.deadline = Some(TokioInstant::now() + lifecycle.timeout);
            Ok(())
        }
        ProcessBrokerEvent::TaskStarted { task: current, .. }
            if current == task && state == LifecycleHookState::Killing =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSpawnFailed { task: current, .. } if current == task => {
            finish_lifecycle_failure(
                client,
                commands,
                config,
                execution,
                "descriptor lifecycle hook could not be started",
            )
            .await
        }
        ProcessBrokerEvent::TaskChild {
            task: current,
            event: child,
        } if current == task && child.is_terminal() => {
            let successful = matches!(child, ChildEvent::Exited { code: 0, .. });
            if state == LifecycleHookState::Killing || !successful {
                return finish_lifecycle_failure(
                    client,
                    commands,
                    config,
                    execution,
                    "descriptor lifecycle hook did not complete successfully",
                )
                .await;
            }
            finish_lifecycle_success(client, commands, config, execution).await
        }
        ProcessBrokerEvent::TaskChild { task: current, .. }
            if current == task && state == LifecycleHookState::Running =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSignalDelivered { task: current }
            if current == task && state == LifecycleHookState::Killing =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSignalFailed { task: current, .. }
            if current == task && state == LifecycleHookState::Killing =>
        {
            finish_lifecycle_failure(
                client,
                commands,
                config,
                execution,
                "descriptor lifecycle hook could not be killed",
            )
            .await
        }
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

async fn finish_lifecycle_success(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let Some(lifecycle) = execution.lifecycle.as_mut() else {
        return Err(ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "completed descriptor lifecycle hook is absent",
        )));
    };
    if lifecycle.kind == LifecycleHookKind::Reload {
        respond_lifecycle(execution.lifecycle.take(), ResponseCode::Ok, None);
        execution.deadline = None;
        return Ok(());
    }
    lifecycle.state = LifecycleHookState::WaitingLifetime;
    execution.pending_stop = lifecycle.after;
    let tracking = config.descriptor_tracking.as_ref().ok_or_else(|| {
        ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "descriptor lifecycle configuration is absent",
        ))
    })?;
    let generation = lifecycle.generation;
    execution.deadline =
        Some(TokioInstant::now() + Duration::from_secs(tracking.lifetime_timeout_seconds));
    finish_descriptor_generation(
        client,
        generation,
        commands.post_exit.as_ref(),
        config,
        execution,
    )
    .await
}

async fn finish_lifecycle_failure(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
    message: &str,
) -> Result<(), ExecutorError> {
    let lifecycle = execution.lifecycle.take().ok_or_else(|| {
        ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "failed descriptor lifecycle hook is absent",
        ))
    })?;
    let generation = lifecycle.generation;
    if lifecycle.kind == LifecycleHookKind::Stop {
        execution.machine.abort_stop(
            lifecycle.generation,
            lifecycle.resume_ready,
            lifecycle.previous_desired,
        )?;
        execution.pending_stop = None;
    }
    execution.deadline = None;
    respond_lifecycle(Some(lifecycle), ResponseCode::Internal, Some(message));
    finish_descriptor_generation(
        client,
        generation,
        commands.post_exit.as_ref(),
        config,
        execution,
    )
    .await
}

fn respond_lifecycle(
    lifecycle: Option<LifecycleExecution>,
    code: ResponseCode,
    message: Option<&str>,
) {
    let Some(mut lifecycle) = lifecycle else {
        return;
    };
    let (Some(command), Some(mut response)) = (lifecycle.command.take(), lifecycle.response.take())
    else {
        return;
    };
    response.code = code;
    if let Some(message) = message {
        message.clone_into(&mut response.message);
    }
    let _ = command.respond(response);
}

async fn handle_service_broker_event(
    client: &mut ProcessBrokerClient,
    event: ProcessBrokerEvent,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    match event {
        ProcessBrokerEvent::Started {
            generation,
            process,
            group,
        } => handle_service_started(generation, process, group, config, execution),
        ProcessBrokerEvent::SpawnFailed { generation, .. }
            if execution.machine.state().live_generation() == Some(generation) =>
        {
            execution.descriptor = None;
            complete_generation(
                client,
                config,
                generation,
                ChildResult::Exited(SPAWN_FAILURE_EXIT),
                true,
                commands.post_exit.as_ref(),
                execution,
            )
            .await
        }
        ProcessBrokerEvent::Child { generation, event }
            if execution.machine.state().live_generation() == Some(generation) =>
        {
            handle_service_child_event(
                client,
                generation,
                event,
                commands.post_exit.as_ref(),
                config,
                execution,
            )
            .await
        }
        ProcessBrokerEvent::SignalDelivered { generation } => {
            finish_signal_request(&mut execution.pending_signals, generation, None)
        }
        ProcessBrokerEvent::SignalFailed {
            generation,
            os_error,
        } => finish_signal_request(&mut execution.pending_signals, generation, os_error),
        ProcessBrokerEvent::GenerationReady { generation }
            if execution.machine.state() == SupervisorState::Running(generation) =>
        {
            publish_readiness(&mut execution.machine, generation, &mut execution.deadline)
        }
        ProcessBrokerEvent::ReadinessFailed { generation, .. }
            if execution.machine.state() == SupervisorState::Running(generation) =>
        {
            handle_readiness_failure(client, generation, commands, config, execution).await
        }
        ProcessBrokerEvent::LifetimeClosed { generation } => {
            handle_lifetime_event(
                client,
                generation,
                LifetimeResult::Closed,
                commands.post_exit.as_ref(),
                config,
                execution,
            )
            .await
        }
        ProcessBrokerEvent::LifetimeFailed { generation } => {
            handle_lifetime_event(
                client,
                generation,
                LifetimeResult::Failed,
                commands.post_exit.as_ref(),
                config,
                execution,
            )
            .await
        }
        ProcessBrokerEvent::Detached { generation } => finish_detach(
            generation,
            true,
            &mut execution.machine,
            &mut execution.status,
            &mut execution.pending_detach,
        ),
        ProcessBrokerEvent::DetachFailed { generation } => finish_detach(
            generation,
            false,
            &mut execution.machine,
            &mut execution.status,
            &mut execution.pending_detach,
        ),
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

fn handle_service_started(
    generation: Generation,
    process: ProcessId,
    group: ProcessGroupId,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if execution.machine.state() == SupervisorState::Starting(generation) {
        execution.machine.child_started(generation)?;
        execution.status.publish_main_pid(process)?;
        if config.readiness.mode == ReadinessMode::Immediate {
            execution.machine.child_ready(generation)?;
            execution.deadline = None;
        } else {
            execution.deadline = Some(
                TokioInstant::now()
                    + Duration::from_secs(config.readiness.timeout_seconds)
                    + BROKER_EVENT_TIMEOUT,
            );
        }
        return Ok(());
    }
    if execution.machine.state() == SupervisorState::Stopping(generation) {
        execution.status.publish_main_pid(process)?;
        return Ok(());
    }
    Err(ExecutorError::UnexpectedBrokerEvent(
        ProcessBrokerEvent::Started {
            generation,
            process,
            group,
        },
    ))
}

async fn handle_readiness_failure(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    execution.status.readiness_failed = true;
    let previous_desired = execution.machine.desired();
    execution.machine.begin_stop(generation)?;
    if config.process_mode == ProcessMode::DescriptorTracking {
        begin_lifecycle_hook(
            client,
            commands.descriptor_stop.as_ref(),
            config,
            execution,
            LifecycleHookRequest {
                after: None,
                command: None,
                generation,
                kind: LifecycleHookKind::Stop,
                previous_desired,
                response: None,
                resume_ready: false,
            },
        )
        .await
    } else {
        request_group_stop(client, generation, &mut execution.pending_signals).await?;
        execution.stop_kill_sent = false;
        execution.deadline = Some(TokioInstant::now() + SERVICE_STOP_GRACE);
        Ok(())
    }
}

const fn task_id(event: &ProcessBrokerEvent) -> Option<BrokerTaskId> {
    match event {
        ProcessBrokerEvent::TaskStarted { task, .. }
        | ProcessBrokerEvent::TaskSpawnFailed { task, .. }
        | ProcessBrokerEvent::TaskChild { task, .. }
        | ProcessBrokerEvent::TaskSignalDelivered { task }
        | ProcessBrokerEvent::TaskSignalFailed { task, .. }
        | ProcessBrokerEvent::TaskContainmentFailed { task } => Some(*task),
        ProcessBrokerEvent::Ready
        | ProcessBrokerEvent::Started { .. }
        | ProcessBrokerEvent::SpawnFailed { .. }
        | ProcessBrokerEvent::Child { .. }
        | ProcessBrokerEvent::SignalDelivered { .. }
        | ProcessBrokerEvent::SignalFailed { .. }
        | ProcessBrokerEvent::Detached { .. }
        | ProcessBrokerEvent::DetachFailed { .. }
        | ProcessBrokerEvent::GenerationReady { .. }
        | ProcessBrokerEvent::ReadinessFailed { .. }
        | ProcessBrokerEvent::LifetimeClosed { .. }
        | ProcessBrokerEvent::LifetimeFailed { .. }
        | ProcessBrokerEvent::ContainmentFailed { .. }
        | ProcessBrokerEvent::LoggerInputsClosed
        | ProcessBrokerEvent::ShutdownComplete
        | ProcessBrokerEvent::ShutdownFailed { .. } => None,
    }
}

fn handle_logger_event(
    client: &ProcessBrokerClient,
    event: ProcessBrokerEvent,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let Some(task) = task_id(&event) else {
        return Err(ExecutorError::UnexpectedBrokerEvent(event));
    };
    let position = execution
        .loggers
        .iter()
        .position(|logger| logger.state.task() == Some(task));
    let Some(position) = position else {
        return Err(ExecutorError::UnexpectedBrokerEvent(event));
    };
    let logger = execution
        .loggers
        .get_mut(position)
        .ok_or_else(|| io::Error::other("selected logger disappeared"))?;
    let adapter_restart = LoggerRestartConfig::default();
    let restart = if logger.logger.is_shared_logger() {
        &config.logging.restart
    } else {
        &adapter_restart
    };
    match event {
        ProcessBrokerEvent::TaskStarted { .. }
            if matches!(logger.state, LoggerExecutionState::Starting { .. }) =>
        {
            logger.state = LoggerExecutionState::Running {
                task,
                started_at: Instant::now(),
            };
            Ok(())
        }
        ProcessBrokerEvent::TaskStarted { .. }
            if matches!(logger.state, LoggerExecutionState::Killing { .. }) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSpawnFailed {
            cleanup_pending, ..
        } if matches!(logger.state, LoggerExecutionState::Starting { .. }) => {
            if cleanup_pending.is_some() {
                logger.state = LoggerExecutionState::Killing {
                    task,
                    deadline: TokioInstant::now() + BROKER_EVENT_TIMEOUT,
                };
            } else if execution.logger_shutdown.permits_restart() {
                schedule_logger_restart(logger, client.process(), restart);
            } else {
                logger.state = LoggerExecutionState::Down;
            }
            Ok(())
        }
        ProcessBrokerEvent::TaskChild { event, .. } if event.is_terminal() => {
            if execution.logger_shutdown.permits_restart() {
                schedule_logger_restart(logger, client.process(), restart);
            } else {
                logger.state = LoggerExecutionState::Down;
            }
            Ok(())
        }
        ProcessBrokerEvent::TaskChild { .. }
        | ProcessBrokerEvent::TaskSignalDelivered { .. }
        | ProcessBrokerEvent::TaskSignalFailed { .. } => Ok(()),
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

fn handle_auxiliary_event(
    client: &ProcessBrokerClient,
    event: ProcessBrokerEvent,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if execution.auxiliary.is_hook() {
        handle_post_exit_event(client, event, config, execution)
    } else {
        handle_condition_event(client, event, config, execution)
    }
}

async fn handle_service_child_event(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    event: ChildEvent,
    post_exit: Option<&ProcessCommand>,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    match event {
        ChildEvent::Stopped { .. }
            if matches!(
                execution.machine.state(),
                SupervisorState::Running(_) | SupervisorState::Ready(_)
            ) =>
        {
            execution.machine.child_paused(generation)?;
        }
        ChildEvent::Continued { .. }
            if matches!(execution.machine.state(), SupervisorState::Paused { .. }) =>
        {
            execution.machine.child_continued(generation)?;
        }
        _ => {
            if let Some(result) = event.terminal_result() {
                if config.process_mode == ProcessMode::DescriptorTracking {
                    let descriptor = execution.descriptor.as_mut().filter(|descriptor| {
                        descriptor.generation == generation && descriptor.launcher_result.is_none()
                    });
                    let Some(descriptor) = descriptor else {
                        return Err(ExecutorError::UnexpectedBrokerEvent(
                            ProcessBrokerEvent::Child { generation, event },
                        ));
                    };
                    descriptor.launcher_result = Some(result);
                    execution.status.clear_main_pid();
                    finish_descriptor_generation(client, generation, post_exit, config, execution)
                        .await?;
                } else {
                    complete_generation(
                        client, config, generation, result, false, post_exit, execution,
                    )
                    .await?;
                }
            }
        }
    }
    Ok(())
}

async fn handle_lifetime_event(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    result: LifetimeResult,
    post_exit: Option<&ProcessCommand>,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let descriptor = execution
        .descriptor
        .as_mut()
        .filter(|descriptor| descriptor.generation == generation)
        .ok_or(ExecutorError::UnexpectedBrokerEvent(
            result.broker_event(generation),
        ))?;
    if descriptor.lifetime_result.is_some() {
        return Err(ExecutorError::UnexpectedBrokerEvent(
            result.broker_event(generation),
        ));
    }
    descriptor.lifetime_preceded_launcher = descriptor.launcher_result.is_none();
    descriptor.lifetime_result = Some(result.child_result());
    let lifecycle_stop = execution.lifecycle.as_ref().is_some_and(|lifecycle| {
        lifecycle.generation == generation && lifecycle.kind == LifecycleHookKind::Stop
    });
    if descriptor.launcher_result.is_none()
        && execution.machine.state().live_generation() == Some(generation)
        && !lifecycle_stop
    {
        if !matches!(execution.machine.state(), SupervisorState::Stopping(_)) {
            execution.machine.begin_stop(generation)?;
        }
        request_group_stop(client, generation, &mut execution.pending_signals).await?;
        execution.stop_kill_sent = false;
        execution.deadline = Some(TokioInstant::now() + SERVICE_STOP_GRACE);
    }
    finish_descriptor_generation(client, generation, post_exit, config, execution).await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifetimeResult {
    Closed,
    Failed,
}

impl LifetimeResult {
    const fn broker_event(self, generation: Generation) -> ProcessBrokerEvent {
        match self {
            Self::Closed => ProcessBrokerEvent::LifetimeClosed { generation },
            Self::Failed => ProcessBrokerEvent::LifetimeFailed { generation },
        }
    }

    const fn child_result(self) -> ChildResult {
        match self {
            Self::Closed => ChildResult::LifetimeClosed,
            Self::Failed => ChildResult::LifetimeFailed,
        }
    }
}

async fn finish_descriptor_generation(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    post_exit: Option<&ProcessCommand>,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let Some(descriptor) = execution
        .descriptor
        .filter(|descriptor| descriptor.generation == generation)
    else {
        return Ok(());
    };
    if execution.lifecycle.as_ref().is_some_and(|lifecycle| {
        lifecycle.generation == generation
            && lifecycle.kind == LifecycleHookKind::Stop
            && lifecycle.state != LifecycleHookState::WaitingLifetime
    }) {
        return Ok(());
    }
    let (Some(launcher), Some(lifetime)) = (descriptor.launcher_result, descriptor.lifetime_result)
    else {
        return Ok(());
    };
    let result = if descriptor.lifetime_preceded_launcher || launcher.is_success(&config.restart) {
        lifetime
    } else {
        launcher
    };
    let lifecycle = if execution.lifecycle.as_ref().is_some_and(|lifecycle| {
        lifecycle.generation == generation && lifecycle.kind == LifecycleHookKind::Stop
    }) {
        execution.lifecycle.take()
    } else {
        None
    };
    execution.descriptor = None;
    complete_generation(
        client, config, generation, result, false, post_exit, execution,
    )
    .await?;
    respond_lifecycle(lifecycle, ResponseCode::Ok, None);
    Ok(())
}

fn handle_post_exit_event(
    client: &ProcessBrokerClient,
    event: ProcessBrokerEvent,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let Some(hook) = config.post_exit.as_ref() else {
        return Err(ExecutorError::UnexpectedBrokerEvent(event));
    };
    match event {
        ProcessBrokerEvent::TaskStarted { task, .. }
            if execution.auxiliary == AuxiliaryExecution::HookStarting(task) =>
        {
            execution.auxiliary = AuxiliaryExecution::HookRunning(task);
            execution.deadline =
                Some(TokioInstant::now() + Duration::from_secs(hook.timeout_seconds));
            Ok(())
        }
        ProcessBrokerEvent::TaskStarted { task, .. }
            if execution.auxiliary == AuxiliaryExecution::HookKilling(task) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSpawnFailed { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::HookStarting(current)
                    | AuxiliaryExecution::HookKilling(current)
                    if current == task
            ) =>
        {
            finish_post_exit(client, config, execution)
        }
        ProcessBrokerEvent::TaskChild { task, event }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::HookRunning(current)
                    | AuxiliaryExecution::HookKilling(current)
                    if current == task
            ) && event.is_terminal() =>
        {
            finish_post_exit(client, config, execution)
        }
        ProcessBrokerEvent::TaskChild { task, .. }
            if execution.auxiliary == AuxiliaryExecution::HookRunning(task) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSignalDelivered { task }
            if execution.auxiliary == AuxiliaryExecution::HookKilling(task) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSignalFailed { task, .. }
            if execution.auxiliary == AuxiliaryExecution::HookKilling(task) =>
        {
            finish_post_exit(client, config, execution)
        }
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

fn finish_post_exit(
    client: &ProcessBrokerClient,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let completion = execution.pending_completion.ok_or_else(|| {
        ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "post-exit task has no pending service completion",
        ))
    })?;
    finish_completion(client, config, completion, true, execution)
}

fn handle_condition_event(
    client: &ProcessBrokerClient,
    event: ProcessBrokerEvent,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let Some(condition) = config.start_condition.as_ref() else {
        return Err(ExecutorError::UnexpectedBrokerEvent(event));
    };
    match event {
        ProcessBrokerEvent::TaskStarted { task, .. }
            if execution.auxiliary == AuxiliaryExecution::Starting(task) =>
        {
            execution.auxiliary = AuxiliaryExecution::Running(task);
            execution.deadline =
                Some(TokioInstant::now() + Duration::from_secs(condition.timeout_seconds));
            Ok(())
        }
        ProcessBrokerEvent::TaskStarted { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { task: current, .. } if current == task
            ) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSpawnFailed { task, .. }
            if execution.auxiliary == AuxiliaryExecution::Starting(task) =>
        {
            schedule_condition_retry(client, task, condition, execution);
            Ok(())
        }
        ProcessBrokerEvent::TaskSpawnFailed { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { task: current, .. } if current == task
            ) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskChild { task, event }
            if (matches!(
                execution.auxiliary,
                AuxiliaryExecution::Running(current) if current == task
            ) || matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { task: current, .. } if current == task
            )) && event.is_terminal() =>
        {
            if execution.auxiliary == AuxiliaryExecution::Running(task)
                && matches!(event, ChildEvent::Exited { code: 0, .. })
            {
                execution.condition_tracker.passed();
                execution.auxiliary = AuxiliaryExecution::Passed;
                execution.deadline = Some(TokioInstant::now());
            } else if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Running(_) | AuxiliaryExecution::Killing { retry: true, .. }
            ) {
                schedule_condition_retry(client, task, condition, execution);
            } else {
                execution.auxiliary = AuxiliaryExecution::Idle;
                execution.deadline = None;
            }
            Ok(())
        }
        ProcessBrokerEvent::TaskChild { task, .. }
            if execution.auxiliary == AuxiliaryExecution::Running(task) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSignalDelivered { task }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { task: current, .. } if current == task
            ) =>
        {
            Ok(())
        }
        ProcessBrokerEvent::TaskSignalFailed { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { task: current, .. } if current == task
            ) =>
        {
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { retry: true, .. }
            ) {
                schedule_condition_retry(client, task, condition, execution);
            } else {
                execution.auxiliary = AuxiliaryExecution::Idle;
                execution.deadline = None;
            }
            Ok(())
        }
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

fn schedule_condition_retry(
    client: &ProcessBrokerClient,
    task: BrokerTaskId,
    condition: &StartConditionConfig,
    execution: &mut ExecutionContext,
) {
    let retry = execution.condition_tracker.failed(condition);
    execution.auxiliary = AuxiliaryExecution::Idle;
    execution.deadline = Some(
        TokioInstant::now()
            + jittered_backoff_seed(
                retry.base_delay_seconds,
                retry.jitter_percent,
                task.get(),
                client.process(),
            ),
    );
}

fn publish_readiness(
    machine: &mut StateMachine,
    generation: Generation,
    deadline: &mut Option<TokioInstant>,
) -> Result<(), ExecutorError> {
    machine.child_ready(generation)?;
    *deadline = None;
    Ok(())
}

fn finish_detach(
    generation: Generation,
    succeeded: bool,
    machine: &mut StateMachine,
    runtime_status: &mut RuntimeStatus,
    pending_detach: &mut Option<PendingDetach>,
) -> Result<(), ExecutorError> {
    let unexpected = if succeeded {
        ProcessBrokerEvent::Detached { generation }
    } else {
        ProcessBrokerEvent::DetachFailed { generation }
    };
    let Some(pending) = pending_detach.take() else {
        return Err(ExecutorError::UnexpectedBrokerEvent(unexpected));
    };
    if pending.generation != generation {
        return Err(ExecutorError::UnexpectedBrokerEvent(unexpected));
    }
    if succeeded {
        machine.set_desired(DesiredState::Exit);
        machine.child_detached(generation)?;
        runtime_status.clear_main_pid();
        runtime_status.started_at = None;
        let _ = pending.command.respond(pending.success);
    } else {
        machine.set_desired(pending.previous_desired);
        let mut response = pending.success;
        response.code = ResponseCode::Internal;
        "broker could not detach the requested live generation".clone_into(&mut response.message);
        let _ = pending.command.respond(response);
    }
    Ok(())
}

async fn complete_generation(
    client: &mut ProcessBrokerClient,
    config: &ServiceConfig,
    generation: Generation,
    result: ChildResult,
    start_failed: bool,
    post_exit: Option<&ProcessCommand>,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    execution.status.last_result = Some(result);
    execution.status.last_readiness_failed = execution.status.readiness_failed;
    execution.status.last_start_failed = start_failed;
    execution.status.clear_main_pid();
    execution.status.down_since = Some(Instant::now());
    let runtime_seconds = execution
        .status
        .started_at
        .take()
        .map_or(0, elapsed_seconds);
    let stop = execution.pending_stop.take();
    let operator_stopped = stop.is_some();
    let policy_result = if execution.status.readiness_failed {
        ChildResult::Exited(1)
    } else {
        result
    };
    if !operator_stopped && (start_failed || !policy_result.is_success(&config.restart)) {
        execution.status.failures = execution.status.failures.saturating_add(1);
    }
    let completion = PendingCompletion {
        generation,
        policy_result,
        runtime_seconds,
        stop,
    };
    if execution.machine.desired() == DesiredState::Once {
        execution.machine.set_desired(DesiredState::Down);
    }
    execution.status.readiness_failed = false;
    execution.stop_kill_sent = false;
    if let Some(post_exit) = post_exit {
        execution.machine.child_completed(generation)?;
        let task = allocate_task(execution)?;
        let command = post_exit_command(
            post_exit,
            generation,
            result,
            start_failed,
            execution.status.last_readiness_failed,
        );
        execution.pending_completion = Some(completion);
        execution.auxiliary = AuxiliaryExecution::HookStarting(task);
        client
            .spawn_task(task, command, CHILD_STARTUP_TIMEOUT)
            .await?;
        execution.deadline =
            Some(TokioInstant::now() + CHILD_STARTUP_TIMEOUT + BROKER_EVENT_TIMEOUT);
        return Ok(());
    }
    finish_completion(client, config, completion, false, execution)
}

fn post_exit_command(
    template: &ProcessCommand,
    generation: Generation,
    result: ChildResult,
    start_failed: bool,
    readiness_failed: bool,
) -> ProcessCommand {
    let mut command = template.clone();
    let (kind, status) = match result {
        ChildResult::Exited(status) => ("exit", status),
        ChildResult::Signaled(signal) => ("signal", signal),
        ChildResult::LifetimeClosed => ("lifetime", 0),
        ChildResult::LifetimeFailed => ("lifetime-failed", 0),
    };
    command.environment_variable("IMMORTAL_EXIT_KIND", kind);
    command.environment_variable("IMMORTAL_EXIT_STATUS", status.to_string());
    command.environment_variable("IMMORTAL_GENERATION", generation.get().to_string());
    command.environment_variable(
        "IMMORTAL_START_FAILED",
        if start_failed { "1" } else { "0" },
    );
    command.environment_variable(
        "IMMORTAL_READINESS_FAILED",
        if readiness_failed { "1" } else { "0" },
    );
    command
}

fn finish_completion(
    client: &ProcessBrokerClient,
    config: &ServiceConfig,
    completion: PendingCompletion,
    after_hook: bool,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let decision = completion.stop.map_or_else(
        || {
            execution.tracker.decide(
                completion.policy_result,
                completion.runtime_seconds,
                elapsed_seconds(execution.epoch),
                execution.machine.desired(),
                &config.restart,
            )
        },
        |stop| match stop {
            StopCompletion::Down | StopCompletion::Restart => RestartDecision::StayDown,
            StopCompletion::Halt => RestartDecision::ExitSupervisor,
        },
    );
    if let RestartDecision::ExitFailure(reason) = decision {
        execution.terminal_failure = Some(reason);
    }
    if after_hook {
        execution
            .machine
            .completion_finished(completion.generation, decision)?;
    } else {
        execution
            .machine
            .child_reaped(completion.generation, decision)?;
    }
    execution.auxiliary = AuxiliaryExecution::Idle;
    execution.pending_completion = None;
    execution.deadline = match decision {
        RestartDecision::Restart {
            base_delay_seconds,
            jitter_percent,
        } => Some(
            TokioInstant::now()
                + jittered_backoff(
                    base_delay_seconds,
                    jitter_percent,
                    completion.generation,
                    client.process(),
                ),
        ),
        RestartDecision::StayDown
        | RestartDecision::ExitSupervisor
        | RestartDecision::ExitFailure(_)
        | RestartDecision::Fail(_) => None,
    };
    Ok(())
}

fn finish_signal_request(
    pending: &mut VecDeque<PendingSignal>,
    generation: Generation,
    os_error: Option<i32>,
) -> Result<(), ExecutorError> {
    let Some(signal) = pending.pop_front() else {
        return Err(ExecutorError::UnexpectedBrokerEvent(os_error.map_or(
            ProcessBrokerEvent::SignalDelivered { generation },
            |os_error| ProcessBrokerEvent::SignalFailed {
                generation,
                os_error: Some(os_error),
            },
        )));
    };
    if let PendingSignal::Control {
        command,
        mut success,
    } = signal
    {
        if let Some(os_error) = os_error {
            success.code = ResponseCode::Internal;
            success.message =
                format!("signal delivery failed with operating-system error {os_error}");
        }
        let _ = command.respond(*success);
    }
    Ok(())
}

fn respond_abandoned_signals(pending: &mut VecDeque<PendingSignal>) {
    while let Some(signal) = pending.pop_front() {
        if let PendingSignal::Control {
            command,
            mut success,
        } = signal
        {
            success.code = ResponseCode::Internal;
            "supervisor exited before signal delivery was acknowledged"
                .clone_into(&mut success.message);
            let _ = command.respond(*success);
        }
    }
}

fn outcome(
    machine: &StateMachine,
    tracker: &RestartTracker,
    status: &RuntimeStatus,
    terminal_failure: Option<FailureReason>,
) -> SupervisionOutcome {
    SupervisionOutcome {
        state: machine.state(),
        terminal_failure,
        last_result: status.last_result,
        last_start_failed: status.last_start_failed,
        last_readiness_failed: status.last_readiness_failed,
        starts: tracker.total_starts(),
    }
}

const fn broker_scope(scope: SignalScope) -> BrokerSignalScope {
    match scope {
        SignalScope::Main => BrokerSignalScope::Process,
        SignalScope::Group => BrokerSignalScope::Group,
    }
}

const fn process_signal(signal: Signal) -> ProcessSignal {
    match signal {
        Signal::User1 => ProcessSignal::User1,
        Signal::User2 => ProcessSignal::User2,
        Signal::Alarm => ProcessSignal::Alarm,
        Signal::Continue => ProcessSignal::Continue,
        Signal::Hangup => ProcessSignal::Hangup,
        Signal::Interrupt => ProcessSignal::Interrupt,
        Signal::Kill => ProcessSignal::Kill,
        Signal::TerminalInput => ProcessSignal::TerminalInput,
        Signal::TerminalOutput => ProcessSignal::TerminalOutput,
        Signal::Quit => ProcessSignal::Quit,
        Signal::Stop => ProcessSignal::Stop,
        Signal::Terminate => ProcessSignal::Terminate,
        Signal::WindowChange => ProcessSignal::WindowChange,
    }
}

async fn shutdown_broker(client: &mut ProcessBrokerClient) -> Result<(), ExecutorError> {
    client.shutdown().await?;
    loop {
        match timeout(BROKER_EVENT_TIMEOUT, client.next_event()).await {
            Ok(Ok(ProcessBrokerEvent::ShutdownComplete)) => return Ok(()),
            Ok(Ok(
                ProcessBrokerEvent::Child { .. }
                | ProcessBrokerEvent::TaskStarted { .. }
                | ProcessBrokerEvent::TaskSpawnFailed { .. }
                | ProcessBrokerEvent::TaskChild { .. }
                | ProcessBrokerEvent::TaskSignalDelivered { .. }
                | ProcessBrokerEvent::TaskSignalFailed { .. },
            )) => {}
            Ok(Ok(ProcessBrokerEvent::ShutdownFailed { .. })) => {
                return Err(ExecutorError::BrokerTimedOut("shutdown cleanup"));
            }
            Ok(Ok(event)) => return Err(ExecutorError::UnexpectedBrokerEvent(event)),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(ExecutorError::BrokerTimedOut("shutdown")),
        }
    }
}

fn reap_broker(process: crate::process::ProcessId) -> Result<(), ExecutorError> {
    let deadline = Instant::now() + BROKER_REAP_TIMEOUT;
    loop {
        match reap_any_event() {
            Ok(Some(event)) if event.pid() == process && event.is_terminal() => {
                return match event {
                    ChildEvent::Exited { code: 0, .. } => Ok(()),
                    _ => Err(ExecutorError::BrokerExited(event)),
                };
            }
            Ok(Some(event)) => return Err(ExecutorError::UnexpectedChildEvent(event)),
            Ok(None) => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            let _ = signal(SignalTarget::Process(process), ProcessSignal::Kill);
            let event = wait_for_event(process)?;
            return Err(ExecutorError::BrokerExited(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn elapsed_seconds(start: Instant) -> u64 {
    start.elapsed().as_secs()
}

fn jittered_backoff(
    base_seconds: u64,
    jitter_percent: u8,
    generation: Generation,
    broker: crate::process::ProcessId,
) -> Duration {
    jittered_backoff_seed(base_seconds, jitter_percent, generation.get(), broker)
}

fn jittered_backoff_seed(
    base_seconds: u64,
    jitter_percent: u8,
    seed: u64,
    broker: crate::process::ProcessId,
) -> Duration {
    let spread = base_seconds
        .saturating_mul(u64::from(jitter_percent))
        .saturating_div(100);
    if spread == 0 {
        return Duration::from_secs(base_seconds);
    }
    let width = spread.saturating_mul(2).saturating_add(1);
    let broker_seed = u64::try_from(broker.get()).map_or(0, |value| value);
    let mut seed = seed ^ broker_seed;
    seed ^= seed >> 30;
    seed = seed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^= seed >> 31;
    let offset = seed % width;
    Duration::from_secs(base_seconds.saturating_sub(spread).saturating_add(offset))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Instant};

    use tokio::time::Instant as TokioInstant;

    use super::{
        LoggerExecution, LoggerExecutionState, LoggerShutdownTier, logger_status,
        next_logger_shutdown_tier, schedule_logger_restart, supervision_finished,
    };
    use crate::{
        config::{BackoffConfig, LoggerRestartConfig},
        process::{BrokerLoggerId, BrokerTaskId, ProcessId},
        status::LoggerStatus,
        supervisor::{FailureReason, StateMachine},
    };

    #[test]
    fn logger_status_summarizes_every_runtime_phase() -> Result<(), Box<dyn Error>> {
        let task = BrokerTaskId::new(1).ok_or("invalid test task")?;
        let mut logger = LoggerExecution::new(BrokerLoggerId::new(0, 0));
        assert_eq!(logger_status(&[]), LoggerStatus::NotConfigured);
        assert_eq!(
            logger_status(std::slice::from_ref(&logger)),
            LoggerStatus::Starting
        );

        logger.state = LoggerExecutionState::Backoff {
            deadline: TokioInstant::now(),
        };
        assert_eq!(
            logger_status(std::slice::from_ref(&logger)),
            LoggerStatus::Backoff
        );

        logger.state = LoggerExecutionState::Running {
            task,
            started_at: Instant::now(),
        };
        assert_eq!(
            logger_status(std::slice::from_ref(&logger)),
            LoggerStatus::Ready
        );

        logger.state = LoggerExecutionState::Failed;
        assert_eq!(
            logger_status(std::slice::from_ref(&logger)),
            LoggerStatus::Failed
        );
        Ok(())
    }

    #[test]
    fn logger_shutdown_drains_file_adapters_before_shared_logger() -> Result<(), Box<dyn Error>> {
        let shared_task = BrokerTaskId::new(1).ok_or("invalid shared logger task")?;
        let adapter_task = BrokerTaskId::new(2).ok_or("invalid file adapter task")?;
        let mut shared = LoggerExecution::new(BrokerLoggerId::shared_logger());
        shared.state = LoggerExecutionState::Running {
            task: shared_task,
            started_at: Instant::now(),
        };
        let mut adapter = LoggerExecution::new(BrokerLoggerId::new(0, 0));
        adapter.state = LoggerExecutionState::Running {
            task: adapter_task,
            started_at: Instant::now(),
        };
        let mut loggers = vec![shared, adapter];

        assert_eq!(
            next_logger_shutdown_tier(&loggers, LoggerShutdownTier::FileAdapters),
            Some(LoggerShutdownTier::FileAdapters)
        );
        let adapter = loggers
            .get_mut(1)
            .ok_or("file adapter logger disappeared")?;
        adapter.state = LoggerExecutionState::Down;
        assert_eq!(
            next_logger_shutdown_tier(&loggers, LoggerShutdownTier::FileAdapters),
            Some(LoggerShutdownTier::SharedLogger)
        );
        assert_eq!(
            next_logger_shutdown_tier(&loggers, LoggerShutdownTier::SharedLogger),
            Some(LoggerShutdownTier::SharedLogger)
        );
        let shared = loggers.get_mut(0).ok_or("shared logger disappeared")?;
        shared.state = LoggerExecutionState::Down;
        assert_eq!(
            next_logger_shutdown_tier(&loggers, LoggerShutdownTier::SharedLogger),
            None
        );
        Ok(())
    }

    #[test]
    fn logger_retry_limit_and_stable_runtime_reset_are_independent() -> Result<(), Box<dyn Error>> {
        let broker = ProcessId::new(1).ok_or("invalid test process")?;
        let restart = LoggerRestartConfig {
            max_retries: Some(1),
            backoff: BackoffConfig {
                initial_seconds: 1,
                max_seconds: 1,
                multiplier: 1,
                jitter_percent: 0,
                reset_after_seconds: 10,
            },
        };
        let mut logger = LoggerExecution::new(BrokerLoggerId::new(0, 0));

        schedule_logger_restart(&mut logger, broker, &restart);
        assert!(matches!(logger.state, LoggerExecutionState::Backoff { .. }));
        logger.state = LoggerExecutionState::Down;
        schedule_logger_restart(&mut logger, broker, &restart);
        assert!(matches!(logger.state, LoggerExecutionState::Failed));

        let task = BrokerTaskId::new(1).ok_or("invalid test task")?;
        let started_at = Instant::now()
            .checked_sub(std::time::Duration::from_secs(10))
            .ok_or("test instant underflow")?;
        logger.state = LoggerExecutionState::Running { task, started_at };
        schedule_logger_restart(&mut logger, broker, &restart);
        assert!(matches!(logger.state, LoggerExecutionState::Backoff { .. }));
        assert_eq!(logger.failure_streak, 1);
        Ok(())
    }

    #[test]
    fn foreground_terminal_detection_distinguishes_initial_down_from_prestart_failure()
    -> Result<(), Box<dyn Error>> {
        let down = StateMachine::default();
        assert!(!supervision_finished(&down, false, true));
        assert!(supervision_finished(&down, false, false));

        let mut failed = StateMachine::default();
        failed.fail_without_child(FailureReason::LoggerRetryLimit)?;
        assert!(supervision_finished(&failed, false, true));
        assert!(!supervision_finished(&failed, true, true));
        Ok(())
    }
}
