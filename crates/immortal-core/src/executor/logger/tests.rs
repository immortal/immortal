//! Tests for which logger failure status reports.
//!
//! Status promises the most recent failure among loggers which have not run
//! again, and a failed start's cause must survive only the reap of the task
//! that start left behind. These tests drive the same event and timer
//! handlers the executor uses, so a cause recorded for one task cannot stand
//! in for a later task's failure.

use std::{
    error::Error,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::{
    super::{
        AuxiliaryExecution, DesiredState, ExecutorError, FailureReason, LoggerExecutionState,
        LoggerShutdownState, LoggerShutdownTier, SupervisorState,
        test_support::{
            ADAPTER, CONDITION, EXECUTE_FAILED, Fixture, SHARED, START_TIMED_OUT, reap,
            time_out_shared_start,
        },
    },
    fail_childless_start_on_logger_exhaustion, handle_due_logger_timers,
};
use crate::{
    config::parse_str,
    process::{BrokerTaskId, ProcessBrokerEvent, SpawnFailure, SpawnStage},
    status::LoggerLastResult,
};

/// Fail the shared logger's start and the adapter's run, in the given order.
fn fail_both(fixture: &mut Fixture, shared_first: bool) -> Result<(), Box<dyn Error>> {
    let shared = fixture.begin_start(SHARED, 1)?;
    let adapter = fixture.begin_start(ADAPTER, 2)?;
    fixture.started(adapter)?;
    if shared_first {
        fixture.spawn_failed(shared, false)?;
        fixture.exited(adapter, 3)?;
    } else {
        fixture.exited(adapter, 3)?;
        fixture.spawn_failed(shared, false)?;
    }
    Ok(())
}

/// Regression: status named the first logger with a failure, so an older
/// shared-logger failure hid a later adapter failure.
#[tokio::test]
async fn status_reports_the_later_failure_whichever_logger_it_belongs_to()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fail_both(&mut fixture, true)?;
    assert_eq!(fixture.reported(), Some(LoggerLastResult::Exited(3)));

    let mut fixture = Fixture::new()?;
    fail_both(&mut fixture, false)?;
    assert_eq!(fixture.reported(), Some(EXECUTE_FAILED));
    Ok(())
}

#[tokio::test]
async fn a_recovered_logger_stops_being_reported_and_the_other_remains()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fail_both(&mut fixture, true)?;

    let adapter = fixture.begin_start(ADAPTER, 3)?;
    fixture.started(adapter)?;
    assert_eq!(
        fixture.reported(),
        Some(EXECUTE_FAILED),
        "the adapter runs again, so only the shared logger's failure remains"
    );

    let shared = fixture.begin_start(SHARED, 4)?;
    fixture.started(shared)?;
    assert_eq!(fixture.reported(), None);
    Ok(())
}

#[tokio::test]
async fn the_reap_of_a_failed_start_keeps_the_spawn_cause() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = fixture.begin_start(SHARED, 1)?;
    fixture.spawn_failed(task, true)?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { .. }
    ));

    fixture.killed(task)?;

    assert_eq!(fixture.reported(), Some(EXECUTE_FAILED));
    Ok(())
}

/// Regression: any reap kept an earlier spawn failure, so a later task that
/// exited on its own was reported as the previous task's failed start.
#[tokio::test]
async fn a_later_tasks_exit_replaces_an_earlier_spawn_failure() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let first = fixture.begin_start(SHARED, 1)?;
    fixture.spawn_failed(first, true)?;
    fixture.killed(first)?;

    let second = fixture.begin_start(SHARED, 2)?;
    fixture.exited(second, 1)?;

    assert_eq!(fixture.reported(), Some(LoggerLastResult::Exited(1)));
    Ok(())
}

/// Regression: a start that reached its deadline recorded nothing, so the
/// kill's reap, even after a late start report, kept the failure of the task
/// before it.
#[tokio::test]
async fn a_start_deadline_is_reported_instead_of_the_previous_tasks_failure()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let adapter = fixture.begin_start(ADAPTER, 1)?;
    fixture.started(adapter)?;
    let first = fixture.begin_start(SHARED, 2)?;
    fixture.spawn_failed(first, false)?;
    assert_eq!(fixture.reported(), Some(EXECUTE_FAILED));

    // The next attempt is already past its start deadline.
    let second = fixture.begin_start(SHARED, 3)?;
    handle_due_logger_timers(&mut fixture.client, &mut fixture.execution).await?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { task, .. } if task == second
    ));
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));

    fixture.started(second)?;
    assert_eq!(
        fixture.reported(),
        Some(START_TIMED_OUT),
        "a start reported after the kill does not make the logger healthy"
    );

    fixture.killed(second)?;
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    assert!(matches!(
        fixture.logger(ADAPTER)?.state,
        LoggerExecutionState::Running { started_at, .. } if started_at <= Instant::now()
    ));
    Ok(())
}

/// Regression: the broker can report a failed start after the supervisor's
/// own start deadline killed the task. That report was an unexpected event,
/// which ended supervision instead of retrying the logger.
#[tokio::test]
async fn a_spawn_failure_reported_after_the_start_deadline_retries_the_logger()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;

    fixture.spawn_failed(task, false)?;

    assert!(
        matches!(
            fixture.logger(SHARED)?.state,
            LoggerExecutionState::Backoff { .. }
        ),
        "no child is left to reap, so the retry is scheduled now"
    );
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    Ok(())
}

#[tokio::test]
async fn a_late_spawn_failure_with_a_child_to_collect_waits_for_its_reap()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;

    fixture.spawn_failed(task, true)?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { .. }
    ));

    fixture.killed(task)?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Backoff { .. }
    ));
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    Ok(())
}

#[tokio::test]
async fn a_late_spawn_failure_during_shutdown_leaves_the_logger_down() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture.execution.logger_shutdown = LoggerShutdownState::Complete;

    fixture.spawn_failed(task, false)?;

    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Down
    ));
    Ok(())
}

/// Accepting a late failure for a known task must not accept one for a task
/// no logger owns.
#[tokio::test]
async fn a_spawn_failure_for_an_unowned_task_stays_a_protocol_fault() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    time_out_shared_start(&mut fixture).await?;
    let unowned = BrokerTaskId::new(99).ok_or("invalid test task")?;

    let outcome = fixture.handle(ProcessBrokerEvent::TaskSpawnFailed {
        task: unowned,
        stage: SpawnStage::Execute,
        failure: SpawnFailure::OperatingSystem,
        os_error: Some(2),
        cleanup_pending: None,
    });

    assert!(matches!(
        outcome,
        Err(ExecutorError::UnexpectedBrokerEvent(_))
    ));
    Ok(())
}

/// A fixture whose service waits for its start condition, which is running,
/// while the shared logger has used up its retries.
fn condition_running_beside_a_failed_logger(
    task: u64,
) -> Result<(Fixture, BrokerTaskId), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let task = BrokerTaskId::new(task).ok_or("invalid test task")?;
    fixture.execution.machine.set_desired(DesiredState::Up);
    fixture.execution.machine.begin_start()?;
    fixture.execution.auxiliary = AuxiliaryExecution::Running(task);
    fixture.logger(SHARED)?.state = LoggerExecutionState::Failed;
    Ok((fixture, task))
}

/// Regression: with a logger out of retries and a condition still running,
/// the check cancelled the condition and reported progress, so the event loop
/// started over without ever reading the condition's reap. It then did the
/// same on every pass, a busy loop that only a kill ended.
#[tokio::test]
async fn a_failed_logger_does_not_make_the_loop_spin_on_a_killed_condition()
-> Result<(), Box<dyn Error>> {
    let (mut fixture, task) = condition_running_beside_a_failed_logger(70)?;

    let progressed =
        fail_childless_start_on_logger_exhaustion(&mut fixture.client, &mut fixture.execution)
            .await?;
    assert!(!progressed, "the reap has to be read before anything else");
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Killing { task, retry: false }
    );
    let progressed =
        fail_childless_start_on_logger_exhaustion(&mut fixture.client, &mut fixture.execution)
            .await?;
    assert!(
        !progressed,
        "a task already being killed is only waited for"
    );

    fixture.dispatch(reap(task)?).await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);

    let progressed =
        fail_childless_start_on_logger_exhaustion(&mut fixture.client, &mut fixture.execution)
            .await?;
    assert!(progressed);
    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Failed(FailureReason::LoggerRetryLimit)
    );
    Ok(())
}

/// Regression: while loggers shut down, an elapsed deadline of one logger
/// was still the loop's next wake-up, though nothing acts on it, so the loop
/// woke at once and again until the shutdown deadline passed.
#[tokio::test]
async fn logger_deadlines_do_not_wake_the_loop_while_loggers_shut_down()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.begin_start(SHARED, 80)?;
    let elapsed = fixture.execution.next_deadline();
    assert!(elapsed.is_some(), "a running start deadline is the wake-up");

    let shutdown = TokioInstant::now() + Duration::from_mins(1);
    fixture.execution.logger_shutdown = LoggerShutdownState::Draining {
        deadline: shutdown,
        tier: LoggerShutdownTier::FileAdapters,
    };

    assert_eq!(fixture.execution.next_deadline(), Some(shutdown));
    Ok(())
}
