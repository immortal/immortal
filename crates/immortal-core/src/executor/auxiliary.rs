//! Auxiliary condition and post-exit task handling.
//!
//! Auxiliary broker tasks never own the main service generation. Start
//! conditions can pass, retry with bounded jitter, or be cancelled before the
//! service starts; post-exit hooks run after a generation has already been
//! recorded and only unblock the pending restart decision. Both paths preserve
//! broker task acknowledgement order and never adopt auxiliary PIDs.

use std::{io, time::Duration};

use tokio::time::Instant as TokioInstant;

use super::{
    AuxiliaryExecution, BROKER_EVENT_TIMEOUT, BrokerTaskId, ChildEvent, ExecutionContext,
    ExecutorError, ProcessBrokerClient, ProcessBrokerEvent, ServiceConfig, StartConditionConfig,
    SupervisorState, finish_completion, jittered_backoff_seed,
};

pub(super) fn handle_auxiliary_event(
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

pub(super) fn handle_post_exit_event(
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
        ProcessBrokerEvent::TaskSpawnFailed {
            task,
            cleanup_pending,
            ..
        } if matches!(
            execution.auxiliary,
            AuxiliaryExecution::HookStarting(current)
                | AuxiliaryExecution::HookKilling(current)
                if current == task
        ) =>
        {
            if cleanup_pending.is_none() {
                return finish_post_exit(client, config, execution);
            }
            // The failed start left a child to collect; completion waits for
            // that reap so the service cannot restart beside it.
            if execution.auxiliary == AuxiliaryExecution::HookStarting(task) {
                execution.auxiliary = AuxiliaryExecution::HookKilling(task);
                execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            }
            Ok(())
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
        // A stop or continue the task's own job control queued ahead of its
        // reap is no change of state, also while it is being killed.
        ProcessBrokerEvent::TaskChild { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::HookRunning(current)
                    | AuxiliaryExecution::HookKilling(current)
                    if current == task
            ) =>
        {
            Ok(())
        }
        // A kill the broker could not confirm does not prove the hook is
        // gone: a slow guard helper can still deliver it. Either way
        // completion waits for the reap, bounded by the cleanup deadline
        // armed when the kill was sent.
        ProcessBrokerEvent::TaskSignalDelivered { task }
        | ProcessBrokerEvent::TaskSignalFailed { task, .. }
            if execution.auxiliary == AuxiliaryExecution::HookKilling(task) =>
        {
            Ok(())
        }
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

pub(super) fn finish_post_exit(
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

pub(super) fn handle_condition_event(
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
        ProcessBrokerEvent::TaskSpawnFailed {
            task,
            cleanup_pending,
            ..
        } if execution.auxiliary == AuxiliaryExecution::Starting(task) => {
            if cleanup_pending.is_some() {
                // The failed start left a child to collect; the retry waits
                // for that reap so two evaluations never run together.
                execution.auxiliary = AuxiliaryExecution::Killing { task, retry: true };
                execution.deadline = Some(TokioInstant::now() + BROKER_EVENT_TIMEOUT);
            } else {
                schedule_condition_retry(client, task, condition, execution);
            }
            Ok(())
        }
        ProcessBrokerEvent::TaskSpawnFailed {
            task,
            cleanup_pending,
            ..
        } if matches!(
            execution.auxiliary,
            AuxiliaryExecution::Killing { task: current, .. } if current == task
        ) =>
        {
            // With a child to collect the reap ends the kill. Without one no
            // reap will follow, so the kill ends here.
            if cleanup_pending.is_none() {
                finish_condition_kill(client, task, condition, execution);
            }
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
        // A stop or continue the task's own job control queued ahead of its
        // reap is no change of state, also while it is being killed.
        ProcessBrokerEvent::TaskChild { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Running(current)
                    | AuxiliaryExecution::Killing { task: current, .. }
                    if current == task
            ) =>
        {
            Ok(())
        }
        // A kill the broker could not confirm does not prove the evaluation
        // is gone: a slow guard helper can still deliver it. Either way the
        // task is kept until its reap, bounded by the cleanup deadline armed
        // when the kill was sent.
        ProcessBrokerEvent::TaskSignalDelivered { task }
        | ProcessBrokerEvent::TaskSignalFailed { task, .. }
            if matches!(
                execution.auxiliary,
                AuxiliaryExecution::Killing { task: current, .. } if current == task
            ) =>
        {
            Ok(())
        }
        event => Err(ExecutorError::UnexpectedBrokerEvent(event)),
    }
}

/// End a killed evaluation: retry it if the kill was a timeout, otherwise
/// leave the condition idle.
fn finish_condition_kill(
    client: &ProcessBrokerClient,
    task: BrokerTaskId,
    condition: &StartConditionConfig,
    execution: &mut ExecutionContext,
) {
    if matches!(
        execution.auxiliary,
        AuxiliaryExecution::Killing { retry: true, .. }
    ) {
        schedule_condition_retry(client, task, condition, execution);
    } else {
        execution.auxiliary = AuxiliaryExecution::Idle;
        execution.deadline = None;
    }
}

/// Schedule the next evaluation of a start condition.
///
/// Only a service still waiting for its condition has a next evaluation: once
/// it is down or has exited, a retry timer would fire into a state which has
/// no use for it and end supervision, so the condition just ends.
pub(super) fn schedule_condition_retry(
    client: &ProcessBrokerClient,
    task: BrokerTaskId,
    condition: &StartConditionConfig,
    execution: &mut ExecutionContext,
) {
    if execution.machine.state() != SupervisorState::WaitingCondition {
        execution.auxiliary = AuxiliaryExecution::Idle;
        execution.deadline = None;
        return;
    }
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
