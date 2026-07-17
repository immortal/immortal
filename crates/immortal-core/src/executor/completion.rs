//! Service-generation completion and descriptor lifetime handling.
//!
//! Completion owns the descriptor lifetime/launcher join, readiness publication,
//! detach acknowledgement, signal acknowledgements, and final restart
//! decision. It updates PID-file state and pending control
//! responses only after the broker confirms the corresponding child, lifetime,
//! detach, or signal event, so supervisor state cannot outrun process ownership.

use std::{collections::VecDeque, time::Instant};

use tokio::time::Instant as TokioInstant;

use super::{
    AuxiliaryExecution, BROKER_EVENT_TIMEOUT, CHILD_STARTUP_TIMEOUT, ChildEvent, ChildResult,
    DesiredState, ExecutionContext, ExecutorError, FailureReason, Generation, LifecycleHookKind,
    LifecycleHookState, PendingCompletion, PendingDetach, PendingSignal, ProcessBrokerClient,
    ProcessBrokerEvent, ProcessCommand, ProcessMode, ResponseCode, RestartDecision, RestartTracker,
    RuntimeStatus, SERVICE_STOP_GRACE, ServiceConfig, StateMachine, StopCompletion,
    SupervisionOutcome, SupervisorState, allocate_task, elapsed_seconds, jittered_backoff,
    request_group_stop, respond_lifecycle,
};

pub(super) async fn handle_service_child_event(
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

pub(super) async fn handle_lifetime_event(
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
pub(super) enum LifetimeResult {
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

pub(super) async fn finish_descriptor_generation(
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

pub(super) fn publish_readiness(
    machine: &mut StateMachine,
    generation: Generation,
    deadline: &mut Option<TokioInstant>,
) -> Result<(), ExecutorError> {
    machine.child_ready(generation)?;
    *deadline = None;
    Ok(())
}

pub(super) fn finish_detach(
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

pub(super) async fn complete_generation(
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

pub(super) fn post_exit_command(
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

pub(super) fn finish_completion(
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

pub(super) fn finish_signal_request(
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

pub(super) fn respond_abandoned_signals(pending: &mut VecDeque<PendingSignal>) {
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

pub(super) fn outcome(
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
