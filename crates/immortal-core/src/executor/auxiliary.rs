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
    AuxiliaryExecution, BrokerTaskId, ChildEvent, ExecutionContext, ExecutorError,
    ProcessBrokerClient, ProcessBrokerEvent, ServiceConfig, StartConditionConfig,
    finish_completion, jittered_backoff_seed,
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

pub(super) fn schedule_condition_retry(
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
