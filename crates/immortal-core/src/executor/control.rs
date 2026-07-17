//! Authenticated control commands and signal conversion.
//!
//! Control handling is serialized in the executor loop after the listener has
//! authenticated clients. Requests are preflighted against pending detach or
//! descriptor hooks, translated through the state-machine contract, and either
//! answered immediately or paired with a pending broker signal acknowledgement.
//! Descriptor-tracked services route stop and HUP through lifecycle hooks rather
//! than raw process signals.

use std::{collections::VecDeque, future::pending, io, time::Duration};

use tokio::{sync::mpsc, time::Instant as TokioInstant};

use super::{
    BROKER_EVENT_TIMEOUT, BrokerSignalScope, CHILD_STARTUP_TIMEOUT, ControlCommand, ControlEffect,
    DesiredState, ExecutionContext, ExecutorError, ExecutorEvent, Generation, LifecycleExecution,
    LifecycleHookKind, LifecycleHookState, LoggerExecutionState, Operation, PendingDetach,
    PendingSignal, PreparedExecution, ProcessBrokerClient, ProcessCommand, ProcessMode,
    ProcessSignal, Response, ResponseCode, SERVICE_STOP_GRACE, ServiceConfig, Signal, SignalScope,
    StateMachine, StopCompletion, SupervisorState, TerminationSignals, TransitionError,
    allocate_task, cancel_auxiliary, decide_request, start_down_loggers,
};

pub(super) async fn next_executor_event(
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

pub(super) async fn receive_control(
    controls: &mut Option<&mut mpsc::Receiver<ControlCommand>>,
) -> Option<ControlCommand> {
    match controls {
        Some(receiver) => receiver.recv().await,
        None => pending().await,
    }
}

pub(super) async fn wait_for_deadline(deadline: Option<TokioInstant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => pending().await,
    }
}

pub(super) async fn begin_supervisor_shutdown(
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

pub(super) async fn apply_control_command(
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

pub(super) async fn apply_standard_control(
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

pub(super) struct AcceptedControl {
    command: ControlCommand,
    effect: ControlEffect,
    previous_desired: DesiredState,
    response: Response,
}

pub(super) async fn apply_descriptor_control(
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

pub(super) struct LifecycleHookRequest {
    pub(super) after: Option<StopCompletion>,
    pub(super) command: Option<ControlCommand>,
    pub(super) generation: Generation,
    pub(super) kind: LifecycleHookKind,
    pub(super) previous_desired: DesiredState,
    pub(super) response: Option<Response>,
    pub(super) resume_ready: bool,
}

pub(super) async fn begin_lifecycle_hook(
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

pub(super) async fn begin_descriptor_shutdown(
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

pub(super) fn generation_is_ready(state: SupervisorState, generation: Generation) -> bool {
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

pub(super) fn reset_failed_loggers(execution: &mut ExecutionContext) -> bool {
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

pub(super) fn control_preflight_rejection(
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

pub(super) async fn begin_group_stop(
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

pub(super) async fn request_group_stop(
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

pub(super) const fn broker_scope(scope: SignalScope) -> BrokerSignalScope {
    match scope {
        SignalScope::Main => BrokerSignalScope::Process,
        SignalScope::Group => BrokerSignalScope::Group,
    }
}

pub(super) const fn process_signal(signal: Signal) -> ProcessSignal {
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
