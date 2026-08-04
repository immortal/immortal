//! Tests spanning executor logger state and foreground terminal detection.

use std::{
    error::Error,
    time::{Duration, Instant},
};

use tokio::net::UnixStream;
use tokio::time::Instant as TokioInstant;

use super::completion::handle_service_child_event;
use super::events::{handle_broker_event, handle_generation_ready, owns_generation};
use super::prepare::prepare_execution;
use super::timers::handle_executor_timer;
use super::{
    DesiredState, ExecutionContext, ExecutorError, LoggerExecution, LoggerExecutionState,
    LoggerShutdownTier, ServiceConfig, advance_childless_state, logger_status,
    next_logger_shutdown_tier, schedule_logger_restart, supervision_finished,
};
use crate::{
    config::{BackoffConfig, LoggerRestartConfig, MAX_SCHEDULE_SECONDS},
    process::{
        BrokerLoggerId, BrokerTaskId, ChildEvent, ProcessBrokerClient, ProcessBrokerEvent,
        ProcessId,
    },
    status::LoggerStatus,
    supervisor::{FailureReason, Generation, RestartDecision, StateMachine, SupervisorState},
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

/// Regression: a reap after a spawn failure aborted the supervisor.
///
/// A spawn that fails after forking leaves a process the broker still has to
/// reap, so it forwards a `Child` event once the supervisor has already handled
/// `SpawnFailed`. By then the lifecycle is in backoff, failure, or a newer
/// generation, and `live_generation` reports none of them, so the event fell
/// through to `UnexpectedBrokerEvent` and exited the supervisor — turning a
/// recoverable spawn failure into supervision loss.
#[test]
fn a_reap_after_spawn_failure_is_stale_rather_than_a_fault() -> Result<(), Box<dyn Error>> {
    let (mut execution, generation) = started_execution()?;
    execution.machine.child_reaped(
        generation,
        RestartDecision::Restart {
            base_delay_seconds: 1,
            jitter_percent: 0,
        },
    )?;

    // The state the supervisor reaches after handling the failure no longer
    // reports a live generation, which is exactly why the event escaped.
    assert!(execution.machine.state().live_generation().is_none());
    assert!(
        execution.machine.issued_generation(generation),
        "the supervisor issued this generation, so its reap is stale bookkeeping"
    );
    Ok(())
}

/// The same holds once retries are exhausted and no generation is owned at all.
#[test]
fn a_reap_after_terminal_failure_is_still_stale() -> Result<(), Box<dyn Error>> {
    let (mut execution, generation) = started_execution()?;
    execution
        .machine
        .child_reaped(generation, RestartDecision::StayDown)?;
    execution
        .machine
        .fail_without_child(FailureReason::RetryLimit)?;

    assert_eq!(
        execution.machine.state(),
        SupervisorState::Failed(FailureReason::RetryLimit)
    );
    assert!(execution.machine.state().generation().is_none());
    assert!(execution.machine.issued_generation(generation));
    Ok(())
}

/// A generation this supervisor never issued stays a protocol fault.
#[test]
fn a_reap_for_an_unissued_generation_is_a_fault() -> Result<(), Box<dyn Error>> {
    let (execution, generation) = started_execution()?;
    let unissued =
        Generation::new(generation.get().saturating_add(1)).ok_or("invalid test generation")?;

    assert!(execution.machine.issued_generation(generation));
    assert!(
        !execution.machine.issued_generation(unissued),
        "a generation above the next one can only come from a broker fault"
    );
    Ok(())
}

/// Drive the real dispatch: the stale reap must be absorbed, not returned.
#[tokio::test]
async fn broker_dispatch_absorbs_a_reap_after_spawn_failure() -> Result<(), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let prepared = prepare_execution(&config, false)?;
    let (supervisor, _broker) = UnixStream::pair()?;
    let mut client = ProcessBrokerClient::for_test(supervisor, broker_process()?);
    let (mut execution, generation) = started_execution()?;
    execution.machine.child_reaped(
        generation,
        RestartDecision::Restart {
            base_delay_seconds: 1,
            jitter_percent: 0,
        },
    )?;

    handle_broker_event(
        &mut client,
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Exited {
                pid: broker_process()?,
                code: 1,
            },
        },
        &prepared.execution,
        &config,
        &mut execution,
    )
    .await?;

    assert!(
        matches!(
            execution.machine.state(),
            SupervisorState::Backoff { generation: current, .. } if current == generation
        ),
        "absorbing the stale reap must leave the backoff untouched"
    );
    Ok(())
}

/// A reap for a generation this supervisor never issued stays a protocol fault.
#[tokio::test]
async fn broker_dispatch_rejects_a_reap_for_an_unissued_generation() -> Result<(), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let prepared = prepare_execution(&config, false)?;
    let (supervisor, _broker) = UnixStream::pair()?;
    let mut client = ProcessBrokerClient::for_test(supervisor, broker_process()?);
    let (mut execution, generation) = started_execution()?;
    let unissued =
        Generation::new(generation.get().saturating_add(1)).ok_or("invalid test generation")?;

    let outcome = handle_broker_event(
        &mut client,
        ProcessBrokerEvent::Child {
            generation: unissued,
            event: ChildEvent::Exited {
                pid: broker_process()?,
                code: 1,
            },
        },
        &prepared.execution,
        &config,
        &mut execution,
    )
    .await;

    assert!(matches!(
        outcome,
        Err(ExecutorError::UnexpectedBrokerEvent(_))
    ));
    Ok(())
}

fn broker_process() -> Result<ProcessId, Box<dyn Error>> {
    ProcessId::new(1).ok_or_else(|| "invalid test process".into())
}

/// Regression: a readiness deadline expiring while paused killed the supervisor.
///
/// `signal stop` during startup pauses a generation which is still awaiting
/// readiness, but the readiness deadline stayed armed. A stopped child cannot
/// declare anything, so the deadline always expired, and no timer arm accepted
/// a paused state — the supervisor died from a documented operator command.
#[tokio::test]
async fn pausing_suspends_the_readiness_deadline() -> Result<(), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let (supervisor, _broker) = UnixStream::pair()?;
    let mut client = ProcessBrokerClient::for_test(supervisor, broker_process()?);
    let (mut execution, generation) = started_execution()?;
    assert!(execution.deadline.is_some());

    handle_service_child_event(
        &mut client,
        generation,
        ChildEvent::Stopped {
            pid: broker_process()?,
            signal: 19,
        },
        None,
        &config,
        &mut execution,
    )
    .await?;

    assert_eq!(
        execution.machine.state(),
        SupervisorState::Paused {
            generation,
            ready: false
        }
    );
    assert!(
        execution.deadline.is_none(),
        "a stopped child cannot declare readiness, so its clock must not run"
    );
    Ok(())
}

/// Resuming an unready generation restarts its readiness wait.
#[tokio::test]
async fn continuing_rearms_the_readiness_deadline() -> Result<(), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let (supervisor, _broker) = UnixStream::pair()?;
    let mut client = ProcessBrokerClient::for_test(supervisor, broker_process()?);
    let (mut execution, generation) = started_execution()?;
    execution.machine.child_paused(generation)?;
    execution.deadline = None;

    handle_service_child_event(
        &mut client,
        generation,
        ChildEvent::Continued {
            pid: broker_process()?,
        },
        None,
        &config,
        &mut execution,
    )
    .await?;

    assert_eq!(
        execution.machine.state(),
        SupervisorState::Running(generation)
    );
    assert!(
        execution.deadline.is_some(),
        "a resumed generation must be given its readiness wait again"
    );
    Ok(())
}

/// A resumed generation which already declared readiness needs no deadline.
#[tokio::test]
async fn continuing_a_ready_generation_arms_no_deadline() -> Result<(), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let (supervisor, _broker) = UnixStream::pair()?;
    let mut client = ProcessBrokerClient::for_test(supervisor, broker_process()?);
    let (mut execution, generation) = started_execution()?;
    execution.machine.child_paused(generation)?;
    handle_generation_ready(generation, &mut execution)?;
    execution.deadline = None;

    handle_service_child_event(
        &mut client,
        generation,
        ChildEvent::Continued {
            pid: broker_process()?,
        },
        None,
        &config,
        &mut execution,
    )
    .await?;

    assert_eq!(
        execution.machine.state(),
        SupervisorState::Ready(generation)
    );
    assert!(execution.deadline.is_none());
    Ok(())
}

/// A deadline surviving into a paused state disarms instead of faulting.
#[tokio::test]
async fn a_timer_in_a_paused_state_disarms_instead_of_faulting() -> Result<(), Box<dyn Error>> {
    let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
    let prepared = prepare_execution(&config, false)?;
    let (supervisor, _broker) = UnixStream::pair()?;
    let mut client = ProcessBrokerClient::for_test(supervisor, broker_process()?);
    let (mut execution, generation) = started_execution()?;
    execution.machine.child_paused(generation)?;
    execution.deadline = Some(TokioInstant::now());

    handle_executor_timer(&mut client, &prepared.execution, &config, &mut execution).await?;

    assert!(execution.deadline.is_none());
    assert_eq!(
        execution.machine.state(),
        SupervisorState::Paused {
            generation,
            ready: false
        }
    );
    Ok(())
}
