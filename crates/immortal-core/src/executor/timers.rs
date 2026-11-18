//! Deadline-driven lifecycle transitions.
//!
//! Timer handling starts delayed services, launches and cancels pre-start
//! conditions, enforces startup/readiness/stop deadlines, and escalates timed
//! out lifecycle hooks. Each transition either mutates the single executor
//! context or asks the broker to signal an owned group, preserving signal order
//! and keeping descriptor lifecycle shutdown serialized. A deadline that only
//! waits for the broker to confirm a start, a readiness outcome, or the end of
//! a killed process never ends supervision when it passes: the wait is reported
//! and kept (`overdue`). Only a state no deadline belongs to stays a fault.

use std::{
    io,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::{
    AuxiliaryExecution, BROKER_EVENT_TIMEOUT, BrokerSignalScope, BrokerTaskId,
    CHILD_STARTUP_TIMEOUT, DescriptorExecution, DesiredState, ExecutionContext, ExecutorError,
    Generation, LifecycleHookState, PendingSignal, PreparedExecution, ProcessBrokerClient,
    ProcessMode, ProcessSignal, ReadinessMode, ServiceConfig, StateMachine, SupervisorState,
    TransitionError, elapsed_seconds, ending, finish_lifecycle_failure, handle_readiness_timer,
    keep_killing_generation, keep_killing_task, keep_waiting,
};

pub(super) fn advance_childless_state(
    machine: &mut StateMachine,
    first_start: &mut bool,
    start_delay_seconds: u64,
    deadline: &mut Option<TokioInstant>,
) -> Result<bool, ExecutorError> {
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
        // Validation bounds `start_delay_seconds`, so this only fails for a
        // delay no monotonic clock can represent. Refusing the start beats
        // aborting the supervisor on an unchecked addition.
        *deadline = Some(TokioInstant::now().checked_add(delay).ok_or_else(|| {
            io::Error::other("start delay exceeds the representable monotonic deadline")
        })?);
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

pub(super) async fn handle_executor_timer(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    match execution.machine.state() {
        SupervisorState::WaitingCondition => {
            handle_waiting_timer(client, commands, config, execution).await?;
        }
        SupervisorState::Backoff { generation, .. } => {
            execution.deadline = None;
            execution.machine.backoff_elapsed(generation)?;
        }
        SupervisorState::Stopping(generation) => {
            handle_stopping_timer(client, generation, execution).await?;
        }
        SupervisorState::Starting(_)
            if ending(&execution.machine, execution.shutdown_requested) =>
        {
            return Err(ExecutorError::BrokerTimedOut("service startup"));
        }
        SupervisorState::Starting(_) => keep_waiting(
            &mut execution.overdue,
            &mut execution.deadline,
            "the service start",
        ),
        SupervisorState::Paused { .. } | SupervisorState::Running(_) => {
            handle_readiness_timer(client, commands, config, execution).await?;
        }
        SupervisorState::Completed(_) => match execution.auxiliary {
            AuxiliaryExecution::HookRunning(task) => {
                execution
                    .task_signals
                    .signal(client, task, ProcessSignal::Kill)
                    .await?;
                execution.auxiliary = AuxiliaryExecution::HookKilling(task);
                execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            }
            AuxiliaryExecution::HookStarting(_)
                if ending(&execution.machine, execution.shutdown_requested) =>
            {
                return Err(ExecutorError::BrokerTimedOut("post-exit hook startup"));
            }
            AuxiliaryExecution::HookStarting(_) => keep_waiting(
                &mut execution.overdue,
                &mut execution.deadline,
                "the post-exit command start",
            ),
            AuxiliaryExecution::HookKilling(task) => {
                keep_killing_auxiliary(client, execution, task, "post-exit hook cleanup").await?;
            }
            _ => {
                return Err(ExecutorError::Transition(TransitionError::Invalid {
                    state: execution.machine.state(),
                    event: "post_exit_timer",
                }));
            }
        },
        // The service went down, exited, or failed while a start condition or
        // hook was still being killed: the expired deadline is that cleanup's.
        SupervisorState::Down | SupervisorState::Exited | SupervisorState::Failed(_) => {
            match execution.auxiliary {
                AuxiliaryExecution::Killing { task, .. } => {
                    keep_killing_auxiliary(client, execution, task, "condition cleanup").await?;
                }
                AuxiliaryExecution::HookKilling(task) => {
                    keep_killing_auxiliary(client, execution, task, "post-exit hook cleanup")
                        .await?;
                }
                _ => {
                    return Err(ExecutorError::Transition(TransitionError::Invalid {
                        state: execution.machine.state(),
                        event: "executor_timer",
                    }));
                }
            }
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

/// Escalate a stop that outlasted its grace to a group kill, then keep
/// waiting for the reap, repeating the kill, unless the supervisor is ending.
async fn handle_stopping_timer(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    if !execution.stop_kill_sent {
        client
            .signal(generation, BrokerSignalScope::Group, ProcessSignal::Kill)
            .await?;
        execution
            .pending_signals
            .push_back(PendingSignal::Lifecycle);
        execution.stop_kill_sent = true;
        execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
        return Ok(());
    }
    if ending(&execution.machine, execution.shutdown_requested) {
        return Err(ExecutorError::BrokerTimedOut("service termination"));
    }
    execution.deadline = Some(
        keep_killing_generation(
            client,
            generation,
            &mut execution.pending_signals,
            &mut execution.overdue,
            "the end of the killed service",
        )
        .await?,
    );
    Ok(())
}

pub(super) async fn handle_lifecycle_timer(
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
            lifecycle.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            execution
                .task_signals
                .signal(client, task, ProcessSignal::Kill)
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
        LifecycleHookState::Starting
            if ending(&execution.machine, execution.shutdown_requested) =>
        {
            Err(ExecutorError::BrokerTimedOut(
                "descriptor lifecycle hook startup",
            ))
        }
        LifecycleHookState::Starting => {
            keep_waiting(
                &mut execution.overdue,
                &mut lifecycle.deadline,
                "the descriptor lifecycle command start",
            );
            Ok(())
        }
        LifecycleHookState::Killing if ending(&execution.machine, execution.shutdown_requested) => {
            Err(ExecutorError::BrokerTimedOut(
                "descriptor lifecycle hook cleanup",
            ))
        }
        LifecycleHookState::Killing => {
            let task = lifecycle.task;
            lifecycle.deadline = Some(
                keep_killing_task(
                    client,
                    task,
                    &mut execution.task_signals,
                    &mut execution.overdue,
                    "the end of the killed descriptor lifecycle command",
                )
                .await?,
            );
            Ok(())
        }
    }
}

/// Keep waiting for the reap of a killed condition or post-exit hook,
/// repeating the kill as [`keep_killing_task`] does, or give up on it as
/// `operation` while the supervisor is ending.
async fn keep_killing_auxiliary(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
    task: BrokerTaskId,
    operation: &'static str,
) -> Result<(), ExecutorError> {
    if ending(&execution.machine, execution.shutdown_requested) {
        return Err(ExecutorError::BrokerTimedOut(operation));
    }
    let what = if operation == "condition cleanup" {
        "the end of the killed start condition"
    } else {
        "the end of the killed post-exit command"
    };
    execution.deadline = Some(
        keep_killing_task(
            client,
            task,
            &mut execution.task_signals,
            &mut execution.overdue,
            what,
        )
        .await?,
    );
    Ok(())
}

pub(super) async fn handle_waiting_timer(
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
            execution
                .task_signals
                .signal(client, task, ProcessSignal::Kill)
                .await?;
            execution.auxiliary = AuxiliaryExecution::Killing { task, retry: true };
            execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            Ok(())
        }
        (Some(_), Some(_), AuxiliaryExecution::Starting(_)) => {
            keep_waiting(
                &mut execution.overdue,
                &mut execution.deadline,
                "the start condition start",
            );
            Ok(())
        }
        (Some(_), Some(_), AuxiliaryExecution::Killing { task, .. }) => {
            keep_killing_auxiliary(client, execution, task, "condition cleanup").await
        }
        _ => Err(ExecutorError::OperatingSystem(io::Error::new(
            io::ErrorKind::InvalidData,
            "prepared condition does not match validated configuration",
        ))),
    }
}

pub(super) async fn cancel_auxiliary(
    client: &mut ProcessBrokerClient,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    // A condition already being killed for a retry is now being cancelled: its
    // reap must end it rather than schedule the retry, which would arm a timer
    // that no state accepts once the service is down or has exited.
    if let AuxiliaryExecution::Killing { task, retry: true } = execution.auxiliary {
        execution.auxiliary = AuxiliaryExecution::Killing { task, retry: false };
        return Ok(());
    }
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
    execution
        .task_signals
        .signal(client, task, ProcessSignal::Kill)
        .await?;
    execution.auxiliary = if hook {
        AuxiliaryExecution::HookKilling(task)
    } else {
        AuxiliaryExecution::Killing { task, retry: false }
    };
    execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
    Ok(())
}

pub(super) fn allocate_task(execution: &mut ExecutionContext) -> io::Result<BrokerTaskId> {
    let task = BrokerTaskId::new(execution.next_task).ok_or_else(|| {
        io::Error::other("pre-start condition task identifier space is exhausted")
    })?;
    execution.next_task = execution
        .next_task
        .checked_add(1)
        .ok_or_else(|| io::Error::other("pre-start condition task identifier overflow"))?;
    Ok(task)
}
pub(super) async fn start_service(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let generation = execution.machine.preconditions_ready()?;
    execution.auxiliary = AuxiliaryExecution::Idle;
    execution
        .tracker
        .record_start(elapsed_seconds(execution.epoch), &config.restart);
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

#[cfg(test)]
mod tests;
