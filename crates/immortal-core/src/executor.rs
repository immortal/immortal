//! Side-effecting foreground supervision driven through the process broker.

use std::{
    collections::VecDeque,
    error::Error,
    fmt::{self, Display, Formatter},
    future::pending,
    io,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

use tokio::{
    runtime::Builder,
    signal::unix::{Signal as UnixSignal, SignalKind, signal as listen_for_signal},
    sync::{mpsc, watch},
    time::{Instant as TokioInstant, timeout},
};

use crate::{
    config::{
        LoggingConfig, ProcessMode, ReadinessConfig, ReadinessMode, RestartPolicy, ServiceConfig,
    },
    control::{
        ControlCommand, ControlEffect, ControlListener, DEFAULT_MAX_CONTROL_CLIENTS, Operation,
        Response, ResponseCode, Signal, SignalScope, StopCompletion, decide_request,
        run_control_server,
    },
    pid_file::OwnedPidFile,
    process::{
        BrokerSignalScope, ChildEvent, DaemonError, DaemonStartup, Daemonized, ProcessBrokerClient,
        ProcessBrokerError, ProcessBrokerEvent, ProcessCommand, ProcessId, ProcessSignal,
        SignalTarget, daemonize, reap_any_event, signal, start_process_broker, wait_for_event,
    },
    runtime::RuntimeOwner,
    status::{LastResult, StatusSnapshot},
    supervisor::{
        ChildResult, DesiredState, Generation, RestartDecision, RestartTracker, StateMachine,
        SupervisorState, TransitionError,
    },
};

const BROKER_EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
const BROKER_REAP_TIMEOUT: Duration = Duration::from_secs(5);
const DAEMON_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SERVICE_STOP_GRACE: Duration = Duration::from_secs(5);
const SPAWN_FAILURE_EXIT: u8 = 127;

/// Final observation returned by the foreground executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisionOutcome {
    /// Terminal supervisor state reached by this foreground invocation.
    pub state: SupervisorState,
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
/// This first operational executor supports direct foreground commands,
/// deterministic environment/path resolution, immediate readiness, restart
/// policy, bounded backoff, and complete broker shutdown. Features whose
/// lifecycle is not yet connected fail closed through [`ExecutorError::Unsupported`].
///
/// # Errors
///
/// Returns configuration-capability, process, broker, or state-transition failures.
pub fn run_foreground(config: &ServiceConfig) -> Result<SupervisionOutcome, ExecutorError> {
    let command = prepare_execution(config, false)?;
    run_prepared(config, None, command, &mut StartupReporter::Foreground)
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
    let command = prepare_execution(config, true)?;
    run_prepared(
        config,
        Some(ControlSetup {
            owner,
            service_name,
        }),
        command,
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
    let command = prepare_execution(config, control_directory.is_some())?;
    match daemonize(DAEMON_STARTUP_TIMEOUT)? {
        Daemonized::Parent { .. } => Ok(DaemonRunOutcome::Parent),
        Daemonized::Daemon(notifier) => {
            let mut startup = StartupReporter::Daemon(Some(notifier));
            let execution = (|| {
                let control = control_directory.map(ControlSetup::acquire).transpose()?;
                run_prepared(config, control, command, &mut startup)
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
) -> Result<ProcessCommand, ExecutorError> {
    validate_supported(config, controlled)?;
    ProcessCommand::from_service(config, std::env::vars_os()).map_err(Into::into)
}

fn run_prepared(
    config: &ServiceConfig,
    control: Option<ControlSetup>,
    command: ProcessCommand,
    startup: &mut StartupReporter,
) -> Result<SupervisionOutcome, ExecutorError> {
    let _supervisor_pid_file = config
        .pid_files
        .supervisor
        .as_deref()
        .map(|path| OwnedPidFile::publish(path, std::process::id()))
        .transpose()?;
    let endpoint = start_process_broker()?;
    let broker_process = endpoint.process();
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let execution = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        wait_for_ready(&mut client).await?;
        let mut signals = SupervisorSignals::new()?;
        let outcome = if let Some(setup) = control {
            drive_controlled_service(&mut client, command, config, &mut signals, setup, startup)
                .await
        } else {
            startup.notify_ready()?;
            drive_service(&mut client, command, config, &mut signals, None, None).await
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

fn validate_supported(config: &ServiceConfig, controlled: bool) -> Result<(), ExecutorError> {
    if !config.enabled {
        return Err(ExecutorError::Unsupported("disabled service execution"));
    }
    if !config.requires.is_empty() || config.start_condition.is_some() {
        return Err(ExecutorError::Unsupported(
            "dependencies and start conditions",
        ));
    }
    if config.post_exit.is_some() {
        return Err(ExecutorError::Unsupported("post-exit hooks"));
    }
    if config.logging != LoggingConfig::default() {
        return Err(ExecutorError::Unsupported("configured logging routes"));
    }
    if config.process_mode != ProcessMode::Foreground {
        return Err(ExecutorError::Unsupported("descriptor-tracking processes"));
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
    command: ProcessCommand,
    config: &ServiceConfig,
    signals: &mut SupervisorSignals,
    setup: ControlSetup,
    startup: &mut StartupReporter,
) -> Result<SupervisionOutcome, ExecutorError> {
    let listener = Arc::new(ControlListener::bind(
        setup.owner.socket(),
        DEFAULT_MAX_CONTROL_CLIENTS,
    )?);
    let (command_sender, mut commands) = mpsc::channel(DEFAULT_MAX_CONTROL_CLIENTS);
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
        command,
        config,
        signals,
        Some(&mut commands),
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
    command: ProcessCommand,
    config: &ServiceConfig,
    signals: &mut SupervisorSignals,
    mut controls: Option<&mut mpsc::Receiver<ControlCommand>>,
    service_name: Option<&str>,
) -> Result<SupervisionOutcome, ExecutorError> {
    let mut execution = ExecutionContext::new(config);

    loop {
        if advance_childless_state(
            &mut execution.machine,
            &mut execution.first_start,
            config.start_delay_seconds,
            &mut execution.deadline,
        )? {
            continue;
        }
        if supervision_finished(&execution.machine, controls.is_some()) {
            respond_abandoned_signals(&mut execution.pending_signals);
            return Ok(outcome(
                &execution.machine,
                &execution.tracker,
                &execution.status,
            ));
        }

        match next_executor_event(client, signals, &mut controls, execution.deadline).await? {
            ExecutorEvent::Timer => {
                handle_executor_timer(client, &command, &mut execution, &config.readiness).await?;
            }
            ExecutorEvent::Shutdown => {
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
            ExecutorEvent::Control(command) => {
                let name = service_name.ok_or(ExecutorError::ControlServerStopped)?;
                apply_control_command(client, name, command, &mut execution).await?;
            }
            ExecutorEvent::ControlClosed => return Err(ExecutorError::ControlServerStopped),
            ExecutorEvent::Broker(event) => {
                handle_broker_event(client, event, config, &mut execution).await?;
            }
        }
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

fn supervision_finished(machine: &StateMachine, controlled: bool) -> bool {
    machine.state() == SupervisorState::Exiting
        || (!controlled
            && matches!(
                machine.state(),
                SupervisorState::Down | SupervisorState::Failed(_)
            ))
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
    deadline: Option<TokioInstant>,
    epoch: Instant,
    first_start: bool,
    machine: StateMachine,
    pending_detach: Option<PendingDetach>,
    pending_signals: VecDeque<PendingSignal>,
    pending_stop: Option<StopCompletion>,
    status: RuntimeStatus,
    stop_kill_sent: bool,
    tracker: RestartTracker,
}

impl ExecutionContext {
    fn new(config: &ServiceConfig) -> Self {
        Self {
            deadline: None,
            epoch: Instant::now(),
            first_start: true,
            machine: StateMachine::default(),
            pending_detach: None,
            pending_signals: VecDeque::new(),
            pending_stop: None,
            status: RuntimeStatus::new(config),
            stop_kill_sent: false,
            tracker: RestartTracker::default(),
        }
    }
}

async fn handle_executor_timer(
    client: &mut ProcessBrokerClient,
    command: &ProcessCommand,
    execution: &mut ExecutionContext,
    readiness: &ReadinessConfig,
) -> Result<(), ExecutorError> {
    match execution.machine.state() {
        SupervisorState::Waiting => {
            let generation = execution.machine.preconditions_ready()?;
            execution
                .tracker
                .record_start(elapsed_seconds(execution.epoch));
            execution.status.started_at = Some(Instant::now());
            execution.status.down_since = None;
            execution.status.readiness_failed = false;
            if readiness.mode == ReadinessMode::Immediate {
                client
                    .spawn(generation, command.clone(), CHILD_STARTUP_TIMEOUT)
                    .await?;
            } else {
                client
                    .spawn_with_readiness(
                        generation,
                        command.clone(),
                        CHILD_STARTUP_TIMEOUT,
                        Duration::from_secs(readiness.timeout_seconds),
                    )
                    .await?;
            }
            execution.deadline =
                Some(TokioInstant::now() + CHILD_STARTUP_TIMEOUT + BROKER_EVENT_TIMEOUT);
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
        SupervisorState::Started(_) => {
            return Err(ExecutorError::BrokerTimedOut("service readiness"));
        }
        state => {
            return Err(ExecutorError::Transition(TransitionError::Invalid {
                state,
                event: "executor_timer",
            }));
        }
    }
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
        snapshot.command.clone_from(&self.command);
        snapshot
    }
}

async fn next_executor_event(
    client: &mut ProcessBrokerClient,
    signals: &mut SupervisorSignals,
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
        SupervisorState::Waiting | SupervisorState::Backoff { .. } => {
            machine.cancel_pending()?;
            *deadline = None;
        }
        SupervisorState::Starting(generation)
        | SupervisorState::Started(generation)
        | SupervisorState::Ready(generation) => {
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
        SupervisorState::Down | SupervisorState::Failed(_) | SupervisorState::Exiting => {}
    }
    Ok(())
}

async fn apply_control_command(
    client: &mut ProcessBrokerClient,
    service_name: &str,
    command: ControlCommand,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if execution.pending_detach.is_some() && command.request().operation != Operation::Status {
        let response = Response {
            code: ResponseCode::Conflict,
            generation: execution.machine.state().generation(),
            message: "a live-child detach operation is already pending".to_owned(),
            status: None,
        };
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
        ));
    }
    match decision.effect {
        ControlEffect::None | ControlEffect::BeginStart => {}
        ControlEffect::CancelPending { .. } => {
            execution.machine.cancel_pending()?;
            execution.deadline = None;
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
            if matches!(
                execution.machine.state(),
                SupervisorState::Waiting | SupervisorState::Backoff { .. }
            ) {
                execution.machine.cancel_pending()?;
                execution.deadline = None;
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
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    match event {
        ProcessBrokerEvent::Started {
            generation,
            process,
            ..
        } if execution.machine.state() == SupervisorState::Starting(generation) => {
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
            Ok(())
        }
        ProcessBrokerEvent::Started {
            generation,
            process,
            ..
        } if execution.machine.state() == SupervisorState::Stopping(generation) => {
            execution.status.publish_main_pid(process)?;
            Ok(())
        }
        ProcessBrokerEvent::SpawnFailed { generation, .. }
            if execution.machine.state().live_generation() == Some(generation) =>
        {
            complete_generation(
                client,
                config,
                generation,
                ChildResult::Exited(SPAWN_FAILURE_EXIT),
                true,
                execution,
            )
        }
        ProcessBrokerEvent::Child { generation, event }
            if execution.machine.state().live_generation() == Some(generation) =>
        {
            if let Some(result) = event.terminal_result() {
                complete_generation(client, config, generation, result, false, execution)?;
            }
            Ok(())
        }
        ProcessBrokerEvent::SignalDelivered { generation } => {
            finish_signal_request(&mut execution.pending_signals, generation, None)
        }
        ProcessBrokerEvent::SignalFailed {
            generation,
            os_error,
        } => finish_signal_request(&mut execution.pending_signals, generation, os_error),
        ProcessBrokerEvent::GenerationReady { generation }
            if execution.machine.state() == SupervisorState::Started(generation) =>
        {
            publish_readiness(&mut execution.machine, generation, &mut execution.deadline)
        }
        ProcessBrokerEvent::ReadinessFailed { generation, .. }
            if execution.machine.state() == SupervisorState::Started(generation) =>
        {
            execution.status.readiness_failed = true;
            execution.machine.begin_stop(generation)?;
            request_group_stop(client, generation, &mut execution.pending_signals).await?;
            execution.stop_kill_sent = false;
            execution.deadline = Some(TokioInstant::now() + SERVICE_STOP_GRACE);
            Ok(())
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
    let pending = pending_detach
        .take()
        .ok_or_else(|| ExecutorError::UnexpectedBrokerEvent(unexpected.clone()))?;
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

fn complete_generation(
    client: &ProcessBrokerClient,
    config: &ServiceConfig,
    generation: Generation,
    result: ChildResult,
    start_failed: bool,
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
    let operator_stopped = execution.pending_stop.is_some();
    let policy_result = if execution.status.readiness_failed {
        ChildResult::Exited(1)
    } else {
        result
    };
    if !operator_stopped && (start_failed || !policy_result.is_success(&config.restart)) {
        execution.status.failures = execution.status.failures.saturating_add(1);
    }
    let decision = execution.pending_stop.take().map_or_else(
        || {
            execution.tracker.decide(
                policy_result,
                runtime_seconds,
                elapsed_seconds(execution.epoch),
                execution.machine.desired(),
                &config.restart,
            )
        },
        |completion| match completion {
            StopCompletion::Down | StopCompletion::Restart => RestartDecision::StayDown,
            StopCompletion::Halt => RestartDecision::ExitSupervisor,
        },
    );
    if execution.machine.desired() == DesiredState::Once {
        execution.machine.set_desired(DesiredState::Down);
    }
    execution.machine.child_reaped(generation, decision)?;
    execution.status.readiness_failed = false;
    execution.stop_kill_sent = false;
    execution.deadline = match decision {
        RestartDecision::Restart {
            base_delay_seconds,
            jitter_percent,
        } => Some(
            TokioInstant::now()
                + jittered_backoff(
                    base_delay_seconds,
                    jitter_percent,
                    generation,
                    client.process(),
                ),
        ),
        RestartDecision::StayDown | RestartDecision::ExitSupervisor | RestartDecision::Fail(_) => {
            None
        }
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
) -> SupervisionOutcome {
    SupervisionOutcome {
        state: machine.state(),
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
            Ok(Ok(ProcessBrokerEvent::Child { .. })) => {}
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
    let spread = base_seconds
        .saturating_mul(u64::from(jitter_percent))
        .saturating_div(100);
    if spread == 0 {
        return Duration::from_secs(base_seconds);
    }
    let width = spread.saturating_mul(2).saturating_add(1);
    let broker_seed = u64::try_from(broker.get()).map_or(0, |value| value);
    let mut seed = generation.get() ^ broker_seed;
    seed ^= seed >> 30;
    seed = seed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^= seed >> 31;
    let offset = seed % width;
    Duration::from_secs(base_seconds.saturating_sub(spread).saturating_add(offset))
}

struct SupervisorSignals {
    terminate: UnixSignal,
    interrupt: UnixSignal,
}

impl SupervisorSignals {
    fn new() -> io::Result<Self> {
        Ok(Self {
            terminate: listen_for_signal(SignalKind::terminate())?,
            interrupt: listen_for_signal(SignalKind::interrupt())?,
        })
    }

    async fn recv(&mut self) -> io::Result<()> {
        let received = tokio::select! {
            received = self.terminate.recv() => received,
            received = self.interrupt.recv() => received,
        };
        received.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "supervisor termination signal stream closed",
            )
        })
    }
}
