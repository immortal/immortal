//! Deadline-driven lifecycle transitions.
//!
//! Timer handling starts delayed services, launches and cancels pre-start
//! conditions, enforces startup/readiness/stop deadlines, and escalates timed
//! out lifecycle hooks. Each transition either mutates the single executor
//! context or asks the broker to signal an owned group, preserving signal order
//! and keeping descriptor lifecycle shutdown serialized.

use std::{
    io,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::{
    AuxiliaryExecution, BROKER_EVENT_TIMEOUT, BrokerSignalScope, BrokerTaskId,
    CHILD_STARTUP_TIMEOUT, DescriptorExecution, DesiredState, ExecutionContext, ExecutorError,
    LifecycleHookState, PendingSignal, PreparedExecution, ProcessBrokerClient, ProcessMode,
    ProcessSignal, ReadinessMode, ServiceConfig, StateMachine, SupervisorState, TransitionError,
    elapsed_seconds, finish_lifecycle_failure,
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

pub(super) async fn cancel_auxiliary(
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
