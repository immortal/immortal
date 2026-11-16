//! Tests for how the broker-event dispatch treats the tasks of conditions,
//! hooks, and loggers.
//!
//! The acknowledgement of a signal the supervisor sent is consumed once, also
//! after the task's reap, and a condition or hook is kept until its reap after
//! a failed start that left a child or a kill the broker could not confirm.
//! A stop or continue of a task's own job control changes nothing while the
//! task runs or is being killed, and each cleanup deadline fails supervision
//! closed. Events for a task no state owns stay protocol faults.

use std::{error::Error, time::Duration};

use tokio::time::Instant as TokioInstant;

use super::super::{
    AuxiliaryExecution, DesiredState, ExecutorError, FailureReason, Generation, LifecycleExecution,
    LifecycleHookKind, LifecycleHookState, LoggerExecutionState, LoggerShutdownState,
    LoggerShutdownTier, ProcessSignal, SupervisorState,
    logger::signal_logger_tier,
    prepare::prepare_execution,
    test_support::{
        CONDITION, Fixture, POST_EXIT, SHARED, START_TIMED_OUT, cancelled_condition, continued,
        failed_start, kill_failed, process, reap, reload_hook, stopped, time_out_shared_start,
    },
    timers::{cancel_auxiliary, handle_executor_timer},
};
use crate::{
    config::parse_str,
    process::{BrokerTaskId, ChildEvent, ProcessBrokerEvent, SpawnFailure, SpawnStage},
};

/// Regression: after a late start failure the logger no longer named the
/// task, so the broker's answer to the supervisor's own kill was routed as an
/// event nobody owned and ended supervision.
#[tokio::test]
async fn the_kill_acknowledgement_is_consumed_after_a_late_spawn_failure()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSpawnFailed {
            task,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
            cleanup_pending: None,
        })
        .await?;

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Backoff { .. }
    ));
    assert_eq!(fixture.execution.task_signals.len(), 0);
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    Ok(())
}

/// The process can die on its own before the broker handles the kill, so the
/// acknowledgement trails the reap.
#[tokio::test]
async fn the_kill_acknowledgement_is_consumed_after_the_reap() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code: 1,
            },
        })
        .await?;

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert_eq!(fixture.execution.task_signals.len(), 0);
    Ok(())
}

/// Each signal is acknowledged once: the acknowledgement of the kill is
/// consumed, and a second one answers nothing this supervisor sent.
#[tokio::test]
async fn an_acknowledgement_that_is_not_owed_stays_a_protocol_fault() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await?;

    let repeated = fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await;

    assert!(repeated.is_err());
    Ok(())
}

/// A shutdown tier sends TERM and CONT, so two acknowledgements are owed for
/// each logger and both are consumed, also after the logger was reaped.
#[tokio::test]
async fn shutdown_signals_owe_one_acknowledgement_each() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = fixture.begin_start(SHARED, 1)?;
    fixture.started(task)?;
    fixture.execution.logger_shutdown = LoggerShutdownState::Complete;
    signal_logger_tier(
        &mut fixture.client,
        &mut fixture.execution,
        LoggerShutdownTier::SharedLogger,
        ProcessSignal::Terminate,
    )
    .await?;
    assert_eq!(fixture.execution.task_signals.len(), 2);

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await?;
    fixture.killed(task)?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert_eq!(fixture.execution.task_signals.len(), 0);
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Down
    ));
    Ok(())
}

/// Regression: only logger tasks had their acknowledgements tracked. A start
/// condition that was cancelled and reaped before the broker answered its
/// kill left an answer nobody owned, which ended supervision.
#[tokio::test]
async fn a_cancelled_conditions_kill_acknowledgement_is_consumed_after_its_reap()
-> Result<(), Box<dyn Error>> {
    let (mut fixture, task) = cancelled_condition(40).await?;
    fixture.dispatch(reap(task)?).await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);

    fixture.dispatch(kill_failed(task)).await?;
    assert_eq!(fixture.execution.task_signals.len(), 0);
    assert!(
        fixture.dispatch(kill_failed(task)).await.is_err(),
        "an answer that is not owed is a fault"
    );
    Ok(())
}

/// Regression: a kill the broker could not confirm was taken as proof that
/// the evaluation was gone. A slow guard helper can still deliver it, so the
/// condition was given up, and could be retried, while its process lived, and
/// the reap that followed ended supervision. The task is kept until its reap.
#[tokio::test]
async fn a_condition_whose_kill_was_not_confirmed_is_kept_until_its_reap()
-> Result<(), Box<dyn Error>> {
    let (mut fixture, task) = cancelled_condition(41).await?;
    let cleanup_deadline = fixture.execution.deadline;

    fixture.dispatch(kill_failed(task)).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Killing { task, retry: false }
    );
    assert_eq!(
        fixture.execution.deadline, cleanup_deadline,
        "the cleanup deadline armed with the kill still bounds the wait"
    );

    fixture.dispatch(reap(task)?).await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);
    assert!(
        fixture.dispatch(reap(task)?).await.is_err(),
        "a task is reaped once; a second reap is a fault"
    );
    Ok(())
}

/// Regression: a start that failed after forking leaves a child to collect.
/// The condition was retried at once, beside that child, whose reap then
/// ended supervision.
#[tokio::test]
async fn a_condition_whose_start_left_a_child_is_retried_only_after_its_reap()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let task = BrokerTaskId::new(42).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Starting(task);

    fixture.dispatch(failed_start(task, true)?).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Killing { task, retry: true }
    );
    assert!(fixture.execution.deadline.is_some());

    fixture.dispatch(reap(task)?).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Idle,
        "the retry is scheduled once the child is collected"
    );
    Ok(())
}

/// A failed start with nothing to collect is retried at once and is followed
/// by no reap, so one that arrives anyway stays a fault.
#[tokio::test]
async fn a_reap_after_a_failed_start_without_a_child_stays_a_protocol_fault()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let task = BrokerTaskId::new(43).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Starting(task);

    fixture.dispatch(failed_start(task, false)?).await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);
    assert!(fixture.dispatch(reap(task)?).await.is_err());
    Ok(())
}

/// Regression: a cancelled start that then failed without a child stayed in
/// its killing state, and the kill's answer was what ended it. It ends at the
/// failure; the answer is consumed afterwards and no reap is expected.
#[tokio::test]
async fn a_cancelled_start_that_fails_without_a_child_expects_no_reap() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let task = BrokerTaskId::new(44).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Starting(task);
    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;

    fixture.dispatch(failed_start(task, false)?).await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);

    fixture.dispatch(kill_failed(task)).await?;
    assert_eq!(fixture.execution.task_signals.len(), 0);
    assert!(fixture.dispatch(reap(task)?).await.is_err());
    Ok(())
}

/// Regression: an acknowledgement that answered no signal was accepted while
/// a state still named its task, and a failed one could even end the task.
#[tokio::test]
async fn a_repeated_acknowledgement_is_a_fault_while_the_task_is_still_owned()
-> Result<(), Box<dyn Error>> {
    let (mut fixture, task) = cancelled_condition(45).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await?;

    for repeated in [
        ProcessBrokerEvent::TaskSignalDelivered { task },
        kill_failed(task),
    ] {
        assert!(fixture.dispatch(repeated).await.is_err());
    }
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Killing { task, retry: false }
    );
    Ok(())
}

/// A post-exit hook is kept until its reap after a kill the broker could not
/// confirm and after a failed start that left a child, so the service cannot
/// restart beside it.
#[tokio::test]
async fn a_post_exit_hook_is_kept_until_its_reap() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(POST_EXIT)?;
    let task = BrokerTaskId::new(46).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::HookRunning(task);
    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;
    fixture.dispatch(kill_failed(task)).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::HookKilling(task)
    );

    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(POST_EXIT)?;
    fixture.execution.auxiliary = AuxiliaryExecution::HookStarting(task);
    fixture.dispatch(failed_start(task, true)?).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::HookKilling(task)
    );
    assert!(fixture.execution.deadline.is_some());
    Ok(())
}

/// A descriptor lifecycle hook is kept until its reap in the same two cases,
/// so a reload or stop cannot start another hook beside it and the live
/// service is not affected.
#[tokio::test]
async fn a_lifecycle_hook_is_kept_until_its_reap() -> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(47).ok_or("invalid test task")?;
    let hook = |state| LifecycleExecution {
        after: None,
        command: None,
        generation: Generation::FIRST,
        kind: LifecycleHookKind::Reload,
        previous_desired: DesiredState::Up,
        response: None,
        resume_ready: true,
        state,
        task,
        timeout: Duration::from_secs(10),
    };
    let state = |fixture: &Fixture| fixture.execution.lifecycle.as_ref().map(|hook| hook.state);

    let mut fixture = Fixture::new()?;
    fixture.execution.lifecycle = Some(hook(LifecycleHookState::Killing));
    fixture
        .execution
        .task_signals
        .signal(&mut fixture.client, task, ProcessSignal::Kill)
        .await?;
    fixture.dispatch(kill_failed(task)).await?;
    assert_eq!(state(&fixture), Some(LifecycleHookState::Killing));

    let mut fixture = Fixture::new()?;
    fixture.execution.lifecycle = Some(hook(LifecycleHookState::Starting));
    fixture.dispatch(failed_start(task, true)?).await?;
    assert_eq!(state(&fixture), Some(LifecycleHookState::Killing));
    assert!(fixture.execution.deadline.is_some());
    Ok(())
}

/// Regression: a stop or continue queued ahead of the reap reached a task
/// that was already being killed and ended supervision as an unexpected
/// event, because only a running task accepted it.
#[tokio::test]
async fn job_control_events_do_not_disturb_a_condition_being_killed() -> Result<(), Box<dyn Error>>
{
    let (mut fixture, task) = cancelled_condition(48).await?;
    fixture.dispatch(stopped(task)?).await?;
    fixture.dispatch(continued(task)?).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Killing { task, retry: false }
    );

    fixture.dispatch(reap(task)?).await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);
    Ok(())
}

#[tokio::test]
async fn job_control_events_do_not_disturb_a_post_exit_hook_being_killed()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(POST_EXIT)?;
    let task = BrokerTaskId::new(49).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::HookRunning(task);
    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;

    fixture.dispatch(stopped(task)?).await?;
    fixture.dispatch(continued(task)?).await?;

    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::HookKilling(task)
    );
    Ok(())
}

#[tokio::test]
async fn job_control_events_do_not_disturb_a_lifecycle_hook_being_killed()
-> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(50).ok_or("invalid test task")?;
    let mut fixture = Fixture::new()?;
    fixture.execution.lifecycle = Some(LifecycleExecution {
        after: None,
        command: None,
        generation: Generation::FIRST,
        kind: LifecycleHookKind::Reload,
        previous_desired: DesiredState::Up,
        response: None,
        resume_ready: true,
        state: LifecycleHookState::Killing,
        task,
        timeout: Duration::from_secs(10),
    });

    fixture.dispatch(stopped(task)?).await?;
    fixture.dispatch(continued(task)?).await?;

    assert_eq!(
        fixture.execution.lifecycle.as_ref().map(|hook| hook.state),
        Some(LifecycleHookState::Killing)
    );
    Ok(())
}

/// Job control for a task no state owns is still a protocol fault.
#[tokio::test]
async fn job_control_events_for_an_unowned_task_stay_a_protocol_fault() -> Result<(), Box<dyn Error>>
{
    let (mut fixture, _task) = cancelled_condition(51).await?;
    let stranger = BrokerTaskId::new(52).ok_or("invalid test task")?;
    assert!(fixture.dispatch(stopped(stranger)?).await.is_err());
    Ok(())
}

/// A lifecycle hook whose kill the broker could not confirm is finished only
/// by its reap, which ends it as a failed hook.
#[tokio::test]
async fn a_lifecycle_hook_whose_kill_was_not_confirmed_ends_at_its_reap()
-> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(53).ok_or("invalid test task")?;
    let mut fixture = Fixture::new()?;
    fixture.execution.lifecycle = Some(reload_hook(task, LifecycleHookState::Killing));
    fixture
        .execution
        .task_signals
        .signal(&mut fixture.client, task, ProcessSignal::Kill)
        .await?;

    fixture.dispatch(kill_failed(task)).await?;
    assert!(fixture.execution.lifecycle.is_some(), "kept until the reap");

    fixture.dispatch(reap(task)?).await?;
    assert!(fixture.execution.lifecycle.is_none());
    assert!(fixture.execution.deadline.is_none());
    Ok(())
}

/// The cleanup deadline armed with a kill still fails supervision closed
/// when the reap never comes, now that a failed kill no longer ends the task.
#[tokio::test]
async fn the_cleanup_deadline_of_a_lifecycle_hook_fails_closed() -> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(54).ok_or("invalid test task")?;
    let mut fixture = Fixture::new()?;
    let prepared = prepare_execution(&fixture.config, false)?;
    fixture.execution.lifecycle = Some(reload_hook(task, LifecycleHookState::Killing));
    fixture.execution.deadline = Some(TokioInstant::now());

    let outcome = handle_executor_timer(
        &mut fixture.client,
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await;

    assert!(matches!(
        outcome,
        Err(ExecutorError::BrokerTimedOut(
            "descriptor lifecycle hook cleanup"
        ))
    ));
    Ok(())
}

#[tokio::test]
async fn the_cleanup_deadline_of_a_condition_fails_closed() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let prepared = prepare_execution(&fixture.config, false)?;
    let task = BrokerTaskId::new(55).ok_or("invalid test task")?;
    fixture.execution.machine.set_desired(DesiredState::Up);
    fixture.execution.machine.begin_start()?;
    fixture.execution.auxiliary = AuxiliaryExecution::Killing { task, retry: true };
    fixture.execution.deadline = Some(TokioInstant::now());

    let outcome = handle_executor_timer(
        &mut fixture.client,
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await;

    assert!(matches!(
        outcome,
        Err(ExecutorError::BrokerTimedOut("condition cleanup"))
    ));
    Ok(())
}

/// A fixture whose service waits for its start condition, with that condition
/// being killed for a retry, as a timed-out evaluation is.
fn condition_killed_for_a_retry(task: u64) -> Result<(Fixture, BrokerTaskId), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let task = BrokerTaskId::new(task).ok_or("invalid test task")?;
    fixture.execution.machine.set_desired(DesiredState::Up);
    fixture.execution.machine.begin_start()?;
    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::WaitingCondition
    );
    fixture.execution.auxiliary = AuxiliaryExecution::Killing { task, retry: true };
    fixture.execution.deadline = Some(TokioInstant::now() + Duration::from_secs(5));
    Ok((fixture, task))
}

/// Regression: a stop that cancelled a condition already being killed for a
/// retry left the retry armed. The reap then scheduled it, and the timer fired
/// when the service was down, which no state accepts: supervision ended.
#[tokio::test]
async fn a_condition_cancelled_while_killed_for_a_retry_is_not_retried()
-> Result<(), Box<dyn Error>> {
    let (mut fixture, task) = condition_killed_for_a_retry(60)?;

    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;
    assert_eq!(
        fixture.execution.auxiliary,
        AuxiliaryExecution::Killing { task, retry: false }
    );
    fixture.execution.machine.cancel_pending()?;

    fixture.dispatch(reap(task)?).await?;

    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);
    assert!(
        fixture.execution.deadline.is_none(),
        "no retry timer may be left armed for a service that is down"
    );
    Ok(())
}

/// Whatever route took the service out of its waiting state, the reap of a
/// condition killed for a retry ends the condition instead of arming a timer.
#[tokio::test]
async fn a_condition_reaped_after_the_service_left_waiting_is_not_retried()
-> Result<(), Box<dyn Error>> {
    let (mut fixture, task) = condition_killed_for_a_retry(61)?;
    fixture.execution.machine.cancel_pending()?;

    fixture.dispatch(reap(task)?).await?;

    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);
    assert!(fixture.execution.deadline.is_none());
    Ok(())
}

/// Regression: a cleanup deadline expiring after the service went down or
/// exited was reported as an invalid transition, not as the cleanup that
/// failed to finish.
#[tokio::test]
async fn an_expired_cleanup_deadline_is_reported_as_the_cleanup_after_the_service_stopped()
-> Result<(), Box<dyn Error>> {
    for stopped_as in ["down", "exited", "failed"] {
        let (mut fixture, _task) = condition_killed_for_a_retry(62)?;
        if stopped_as == "exited" {
            fixture.execution.machine.set_desired(DesiredState::Halt);
        }
        fixture.execution.machine.cancel_pending()?;
        if stopped_as == "failed" {
            // A logger out of retries fails the service while the condition's
            // kill is still being waited for.
            fixture
                .execution
                .machine
                .fail_without_child(FailureReason::LoggerRetryLimit)?;
        }
        fixture.execution.deadline = Some(TokioInstant::now());
        let prepared = prepare_execution(&fixture.config, false)?;

        let outcome = handle_executor_timer(
            &mut fixture.client,
            &prepared.execution,
            &fixture.config,
            &mut fixture.execution,
        )
        .await;

        assert!(
            matches!(
                outcome,
                Err(ExecutorError::BrokerTimedOut("condition cleanup"))
            ),
            "{:?} in {:?}",
            outcome.err(),
            fixture.execution.machine.state()
        );
    }
    Ok(())
}
