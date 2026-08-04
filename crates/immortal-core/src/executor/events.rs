//! Broker event routing for service, logger, and lifecycle tasks.
//!
//! Broker events arrive after the broker has applied its ownership checks. This
//! module classifies containment failures first, advances logger-input shutdown,
//! then routes task events to lifecycle hooks, logger pipelines, or auxiliary
//! condition/post-exit handlers before handling service-generation events. Each
//! branch preserves the original event ordering and error precedence.

use std::{
    io,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::{
    BROKER_EVENT_TIMEOUT, BrokerTaskId, ChildEvent, ChildResult, ExecutionContext, ExecutorError,
    Generation, LOGGER_DRAIN_GRACE, LifecycleExecution, LifecycleHookKind, LifecycleHookRequest,
    LifecycleHookState, LifetimeResult, LoggerExecutionState, LoggerRestartConfig,
    LoggerShutdownState, LoggerShutdownTier, PreparedExecution, ProcessBrokerClient,
    ProcessBrokerEvent, ProcessGroupId, ProcessId, ProcessMode, ReadinessMode, ResponseCode,
    SERVICE_STOP_GRACE, SPAWN_FAILURE_EXIT, ServiceConfig, SupervisorState, begin_lifecycle_hook,
    complete_generation, finish_descriptor_generation, finish_detach, finish_signal_request,
    handle_auxiliary_event, handle_lifetime_event, handle_service_child_event, logger_tier_is_down,
    publish_readiness, request_group_stop, schedule_logger_restart,
};

pub(super) async fn handle_broker_event(
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

pub(super) async fn handle_lifecycle_event(
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

pub(super) async fn finish_lifecycle_success(
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

pub(super) async fn finish_lifecycle_failure(
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

pub(super) fn respond_lifecycle(
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

pub(super) async fn handle_service_broker_event(
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
            if owns_generation(execution, generation) =>
        {
            handle_generation_ready(generation, execution)
        }
        ProcessBrokerEvent::ReadinessFailed { generation, .. }
            if execution.machine.state() == SupervisorState::Running(generation) =>
        {
            handle_readiness_failure(client, generation, commands, config, execution).await
        }
        ProcessBrokerEvent::ReadinessFailed { generation, .. }
            if owns_generation(execution, generation) =>
        {
            // The generation already has a lifecycle decision, so a late
            // failure only updates status; its exit or continuation drives the
            // next transition.
            execution.status.readiness_failed = true;
            Ok(())
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

/// Return whether the supervisor still owns `generation`.
///
/// Readiness observations are handled for every owned generation, including one
/// the supervisor has already decided to stop, pause, back off, or complete.
/// Only a generation it does not own at all is a protocol fault.
pub(super) fn owns_generation(execution: &ExecutionContext, generation: Generation) -> bool {
    execution.machine.state().generation() == Some(generation)
}

/// Apply a readiness observation which may arrive after the supervisor moved on.
///
/// The broker's readiness watcher is independent of supervisor state and
/// forwards on generation membership alone, so `stop`, `restart`, or a
/// job-control `stop` during startup races a completed readiness check. A
/// paused generation records readiness in place so continuation resumes ready;
/// any other owned generation already has a lifecycle decision and drops it.
pub(super) fn handle_generation_ready(
    generation: Generation,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if matches!(
        execution.machine.state(),
        SupervisorState::Running(_) | SupervisorState::Paused { ready: false, .. }
    ) {
        return publish_readiness(&mut execution.machine, generation, &mut execution.deadline);
    }
    Ok(())
}

pub(super) fn handle_service_started(
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

pub(super) async fn handle_readiness_failure(
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

pub(super) const fn task_id(event: &ProcessBrokerEvent) -> Option<BrokerTaskId> {
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

pub(super) fn handle_logger_event(
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
