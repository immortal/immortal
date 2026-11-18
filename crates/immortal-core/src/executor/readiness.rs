//! Readiness failures the supervisor could not act on when they arrived.
//!
//! The broker's readiness watcher counts from the start of a generation and
//! keeps counting while job control holds the service: paused time is part of
//! the readiness window. A failure can therefore arrive while the generation is
//! paused, and a descriptor-tracked generation's failure can arrive while a
//! reload hook runs, when no stop may start beside it. Either way the failure is
//! recorded on the generation and acted on as soon as the generation is running
//! again with no hook: right after it continues, and right after a reload hook
//! ends. The broker sends nothing more for that generation, so waiting for
//! another answer instead would wait for ever. A stop hook that fails to stop a
//! generation whose readiness failed is retried after the stop grace, so a hook
//! that keeps failing is retried at a bounded pace rather than in a loop. A
//! shutdown requested meanwhile takes precedence, since it stops the service
//! anyway.

use super::{
    ExecutionContext, ExecutorError, PreparedExecution, ProcessBrokerClient, ServiceConfig,
    SupervisorState, handle_readiness_failure, keep_waiting,
};

/// Handle the generation deadline of a running or paused generation.
///
/// A generation still awaiting the broker's readiness outcome, running or
/// paused, keeps waiting for it. A paused generation whose readiness is
/// settled, declared or failed, waits for nothing until it continues. A
/// running generation whose readiness failed is stopped now, unless a hook
/// still runs, whose end acts on the failure instead.
///
/// # Errors
///
/// Returns the failure to begin the stop.
pub(super) async fn handle_readiness_timer(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    match execution.machine.state() {
        SupervisorState::Running(generation) if execution.status.readiness_failed => {
            if execution.lifecycle.is_some() {
                execution.deadline = None;
                return Ok(());
            }
            handle_readiness_failure(client, generation, commands, config, execution).await
        }
        SupervisorState::Running(_) | SupervisorState::Paused { ready: false, .. }
            if !execution.status.readiness_failed =>
        {
            keep_waiting(
                &mut execution.overdue,
                &mut execution.deadline,
                "the service readiness outcome",
            );
            Ok(())
        }
        _ => {
            execution.deadline = None;
            Ok(())
        }
    }
}

/// Act on a readiness failure recorded while the generation could not stop.
///
/// Does nothing unless the generation is running, its readiness failed, no
/// lifecycle hook runs, and no shutdown is pending.
///
/// # Errors
///
/// Returns the failure to begin the stop.
pub(super) async fn act_on_recorded_readiness_failure(
    client: &mut ProcessBrokerClient,
    commands: &PreparedExecution,
    config: &ServiceConfig,
    execution: &mut ExecutionContext,
) -> Result<(), ExecutorError> {
    let SupervisorState::Running(generation) = execution.machine.state() else {
        return Ok(());
    };
    if !execution.status.readiness_failed
        || execution.lifecycle.is_some()
        || execution.shutdown_requested
    {
        return Ok(());
    }
    handle_readiness_failure(client, generation, commands, config, execution).await
}

#[cfg(test)]
mod tests;
