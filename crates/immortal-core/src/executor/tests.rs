//! Tests spanning executor logger state and foreground terminal detection.

use std::{
    error::Error,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::events::{handle_generation_ready, owns_generation};
use super::{
    DesiredState, ExecutionContext, ExecutorError, LoggerExecution, LoggerExecutionState,
    LoggerShutdownTier, ServiceConfig, advance_childless_state, logger_status,
    next_logger_shutdown_tier, schedule_logger_restart, supervision_finished,
};
use crate::{
    config::{BackoffConfig, LoggerRestartConfig, MAX_SCHEDULE_SECONDS},
    process::{BrokerLoggerId, BrokerTaskId, ProcessId},
    status::LoggerStatus,
    supervisor::{FailureReason, Generation, StateMachine, SupervisorState},
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

/// Drive one execution context to a started, not-yet-ready generation.
fn started_execution() -> Result<(ExecutionContext, Generation), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let mut execution = ExecutionContext::new(&config, Vec::new(), false);
    execution.machine.set_desired(DesiredState::Up);
    execution.machine.begin_start()?;
    let generation = execution.machine.preconditions_ready()?;
    execution.machine.child_started(generation)?;
    execution.deadline = Some(TokioInstant::now() + Duration::from_secs(30));
    Ok((execution, generation))
}

#[test]
fn readiness_publishes_only_for_a_running_generation() -> Result<(), Box<dyn Error>> {
    let (mut execution, generation) = started_execution()?;
    handle_generation_ready(generation, &mut execution)?;
    assert_eq!(
        execution.machine.state(),
        SupervisorState::Ready(generation)
    );
    assert!(execution.deadline.is_none());
    Ok(())
}

#[test]
fn readiness_records_against_a_paused_generation() -> Result<(), Box<dyn Error>> {
    let (mut execution, generation) = started_execution()?;
    execution.machine.child_paused(generation)?;
    handle_generation_ready(generation, &mut execution)?;
    assert_eq!(
        execution.machine.state(),
        SupervisorState::Paused {
            generation,
            ready: true
        }
    );
    execution.machine.child_continued(generation)?;
    assert_eq!(
        execution.machine.state(),
        SupervisorState::Ready(generation)
    );
    Ok(())
}

/// Regression: readiness racing a lifecycle decision aborted the supervisor.
///
/// The broker's readiness watcher is independent of supervisor state, so a
/// `stop` or `restart` during startup delivers readiness for a generation the
/// supervisor is no longer running. Guarding the arm on `Running` alone routed
/// the event to `UnexpectedBrokerEvent` and exited the supervisor.
#[test]
fn readiness_is_dropped_for_a_stopping_generation() -> Result<(), Box<dyn Error>> {
    let (mut execution, generation) = started_execution()?;
    execution.machine.begin_stop(generation)?;
    let deadline = execution.deadline;

    handle_generation_ready(generation, &mut execution)?;
    assert_eq!(
        execution.machine.state(),
        SupervisorState::Stopping(generation)
    );
    assert_eq!(
        execution.deadline, deadline,
        "a dropped readiness observation must not clear the stop deadline"
    );
    Ok(())
}

#[test]
fn readiness_for_an_unowned_generation_is_a_protocol_fault() -> Result<(), Box<dyn Error>> {
    let (mut execution, generation) = started_execution()?;
    let foreign =
        Generation::new(generation.get().saturating_add(1)).ok_or("invalid generation")?;
    assert!(owns_generation(&execution, generation));
    assert!(!owns_generation(&execution, foreign));

    execution.machine.begin_stop(generation)?;
    assert!(owns_generation(&execution, generation));
    assert!(!owns_generation(&execution, foreign));

    let (mut execution, generation) = started_execution()?;
    execution.machine.child_paused(generation)?;
    assert!(owns_generation(&execution, generation));
    assert!(!owns_generation(&execution, foreign));
    Ok(())
}
