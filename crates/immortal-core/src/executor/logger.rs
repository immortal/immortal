//! Logger startup, restart, status, and shutdown orchestration.
//!
//! Logger tasks are broker-owned auxiliary process groups. The shared external
//! logger and local file adapters start before the service can initialize;
//! failures advance independent backoff counters. Shutdown closes service inputs
//! first, drains file adapters before the shared logger, and escalates each tier
//! with bounded terminate/kill deadlines without adopting logger PIDs here.

use std::{io, time::Duration};

use tokio::time::Instant as TokioInstant;

use super::{
    AuxiliaryExecution, BROKER_EVENT_TIMEOUT, BrokerLoggerId, BrokerSignalScope,
    CHILD_STARTUP_TIMEOUT, ExecutionContext, ExecutorError, FailureReason, LOGGER_DRAIN_GRACE,
    LOGGER_PREPARE_GRACE, LOGGER_STOP_GRACE, LoggerExecution, LoggerExecutionState,
    LoggerRestartConfig, LoggerShutdownState, LoggerShutdownTier, LoggerStatus,
    ProcessBrokerClient, ProcessId, ProcessSignal, SupervisorState, allocate_task,
    cancel_auxiliary, jittered_backoff_seed,
};

pub(super) async fn fail_childless_start_on_logger_exhaustion(
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

pub(super) async fn advance_logger_shutdown(
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

pub(super) fn advance_logger_shutdown_tier(execution: &mut ExecutionContext) {
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

pub(super) fn next_logger_shutdown_tier(
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

pub(super) fn logger_tier_is_down(loggers: &[LoggerExecution], tier: LoggerShutdownTier) -> bool {
    loggers
        .iter()
        .filter(|logger| logger_shutdown_tier(logger.logger) == tier)
        .all(|logger| matches!(logger.state, LoggerExecutionState::Down))
}

pub(super) fn logger_shutdown_tier(logger: BrokerLoggerId) -> LoggerShutdownTier {
    if logger.is_shared_logger() {
        LoggerShutdownTier::SharedLogger
    } else {
        LoggerShutdownTier::FileAdapters
    }
}

pub(super) async fn start_down_loggers(
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

pub(super) async fn handle_due_logger_timers(
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

pub(super) async fn signal_logger_tier(
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

pub(super) fn schedule_logger_restart(
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
pub(super) fn logger_status(loggers: &[LoggerExecution]) -> LoggerStatus {
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
