//! Tests spanning executor logger state and foreground terminal detection.

use std::{
    error::Error,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::{
    DesiredState, ExecutorError, LoggerExecution, LoggerExecutionState, LoggerShutdownTier,
    advance_childless_state, logger_status, next_logger_shutdown_tier, schedule_logger_restart,
    supervision_finished,
};
use crate::{
    config::{BackoffConfig, LoggerRestartConfig, MAX_SCHEDULE_SECONDS},
    process::{BrokerLoggerId, BrokerTaskId, ProcessId},
    status::LoggerStatus,
    supervisor::{FailureReason, StateMachine},
};

#[test]
fn logger_status_summarizes_every_runtime_phase() -> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(1).ok_or("invalid test task")?;
    let mut logger = LoggerExecution::new(BrokerLoggerId::new(0, 0));
    assert_eq!(logger_status(&[]), LoggerStatus::NotConfigured);
    assert_eq!(
        logger_status(std::slice::from_ref(&logger)),
        LoggerStatus::Starting
    );

    logger.state = LoggerExecutionState::Backoff {
        deadline: TokioInstant::now(),
    };
    assert_eq!(
        logger_status(std::slice::from_ref(&logger)),
        LoggerStatus::Backoff
    );

    logger.state = LoggerExecutionState::Running {
        task,
        started_at: Instant::now(),
    };
    assert_eq!(
        logger_status(std::slice::from_ref(&logger)),
        LoggerStatus::Ready
    );

    logger.state = LoggerExecutionState::Failed;
    assert_eq!(
        logger_status(std::slice::from_ref(&logger)),
        LoggerStatus::Failed
    );
    Ok(())
}

#[test]
fn logger_shutdown_drains_file_adapters_before_shared_logger() -> Result<(), Box<dyn Error>> {
    let shared_task = BrokerTaskId::new(1).ok_or("invalid shared logger task")?;
    let adapter_task = BrokerTaskId::new(2).ok_or("invalid file adapter task")?;
    let mut shared = LoggerExecution::new(BrokerLoggerId::shared_logger());
    shared.state = LoggerExecutionState::Running {
        task: shared_task,
        started_at: Instant::now(),
    };
    let mut adapter = LoggerExecution::new(BrokerLoggerId::new(0, 0));
    adapter.state = LoggerExecutionState::Running {
        task: adapter_task,
        started_at: Instant::now(),
    };
    let mut loggers = vec![shared, adapter];

    assert_eq!(
        next_logger_shutdown_tier(&loggers, LoggerShutdownTier::FileAdapters),
        Some(LoggerShutdownTier::FileAdapters)
    );
    let adapter = loggers
        .get_mut(1)
        .ok_or("file adapter logger disappeared")?;
    adapter.state = LoggerExecutionState::Down;
    assert_eq!(
        next_logger_shutdown_tier(&loggers, LoggerShutdownTier::FileAdapters),
        Some(LoggerShutdownTier::SharedLogger)
    );
    assert_eq!(
        next_logger_shutdown_tier(&loggers, LoggerShutdownTier::SharedLogger),
        Some(LoggerShutdownTier::SharedLogger)
    );
    let shared = loggers.get_mut(0).ok_or("shared logger disappeared")?;
    shared.state = LoggerExecutionState::Down;
    assert_eq!(
        next_logger_shutdown_tier(&loggers, LoggerShutdownTier::SharedLogger),
        None
    );
    Ok(())
}

#[test]
fn logger_retry_limit_and_stable_runtime_reset_are_independent() -> Result<(), Box<dyn Error>> {
    let broker = ProcessId::new(1).ok_or("invalid test process")?;
    let restart = LoggerRestartConfig {
        max_retries: Some(1),
        backoff: BackoffConfig {
            initial_seconds: 1,
            max_seconds: 1,
            multiplier: 1,
            jitter_percent: 0,
            reset_after_seconds: 10,
        },
    };
    let mut logger = LoggerExecution::new(BrokerLoggerId::new(0, 0));

    schedule_logger_restart(&mut logger, broker, &restart);
    assert!(matches!(logger.state, LoggerExecutionState::Backoff { .. }));
    logger.state = LoggerExecutionState::Down;
    schedule_logger_restart(&mut logger, broker, &restart);
    assert!(matches!(logger.state, LoggerExecutionState::Failed));

    let task = BrokerTaskId::new(1).ok_or("invalid test task")?;
    let started_at = Instant::now()
        .checked_sub(std::time::Duration::from_secs(10))
        .ok_or("test instant underflow")?;
    logger.state = LoggerExecutionState::Running { task, started_at };
    schedule_logger_restart(&mut logger, broker, &restart);
    assert!(matches!(logger.state, LoggerExecutionState::Backoff { .. }));
    assert_eq!(logger.failure_streak, 1);
    Ok(())
}

#[test]
fn foreground_terminal_detection_distinguishes_initial_down_from_prestart_failure()
-> Result<(), Box<dyn Error>> {
    let down = StateMachine::default();
    assert!(!supervision_finished(&down, false, true));
    assert!(supervision_finished(&down, false, false));

    let mut failed = StateMachine::default();
    failed.fail_without_child(FailureReason::LoggerRetryLimit)?;
    assert!(supervision_finished(&failed, false, true));
    assert!(!supervision_finished(&failed, true, true));
    Ok(())
}

#[test]
fn childless_start_schedules_a_bounded_start_delay() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.set_desired(DesiredState::Up);
    let mut first_start = true;
    let mut deadline = None;
    let before = TokioInstant::now();

    assert!(advance_childless_state(
        &mut machine,
        &mut first_start,
        MAX_SCHEDULE_SECONDS,
        &mut deadline,
    )?);
    let scheduled = deadline.ok_or("bounded start delay did not schedule a deadline")?;
    assert!(!first_start);
    assert!(scheduled >= before + Duration::from_secs(MAX_SCHEDULE_SECONDS));
    Ok(())
}

#[test]
fn childless_start_refuses_an_unrepresentable_start_delay() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.set_desired(DesiredState::Up);
    let mut first_start = true;
    let mut deadline = None;

    let error = advance_childless_state(&mut machine, &mut first_start, u64::MAX, &mut deadline)
        .err()
        .ok_or("an unrepresentable start delay was scheduled")?;
    assert!(matches!(error, ExecutorError::OperatingSystem(_)));
    assert!(deadline.is_none());
    Ok(())
}
