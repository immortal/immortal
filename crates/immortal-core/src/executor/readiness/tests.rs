//! Tests for readiness across job control and descriptor lifecycle hooks.
//!
//! Paused time counts toward the readiness window, a failure recorded while
//! the supervisor cannot act on it is acted on as soon as the generation runs
//! again with no hook, and none of this may end supervision.

use std::error::Error;

use tokio::time::Instant as TokioInstant;

use super::super::{
    DesiredState, LifecycleHookKind, LifecycleHookState, SERVICE_STOP_GRACE, SupervisorState,
    prepare::prepare_execution,
    test_support::{Fixture, HOOK_TIMEOUT, process, reload_hook, stop_hook},
    timers::handle_executor_timer,
};
use crate::{
    control::StopCompletion,
    process::{BrokerTaskId, ChildEvent, ProcessBrokerEvent, ReadinessFailure},
    supervisor::Generation,
};

fn stopped(generation: Generation) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::Child {
        generation,
        event: ChildEvent::Stopped {
            pid: process()?,
            signal: 19,
        },
    })
}

fn continued(generation: Generation) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::Child {
        generation,
        event: ChildEvent::Continued { pid: process()? },
    })
}

const fn readiness_failed(generation: Generation) -> ProcessBrokerEvent {
    ProcessBrokerEvent::ReadinessFailed {
        generation,
        failure: ReadinessFailure::Timeout,
    }
}

fn hook_exited(task: BrokerTaskId, code: u8) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::TaskChild {
        task,
        event: ChildEvent::Exited {
            pid: process()?,
            code,
        },
    })
}

/// Fire the generation timer with its deadline already passed.
async fn fire(fixture: &mut Fixture) -> Result<(), Box<dyn Error>> {
    let prepared = prepare_execution(&fixture.config, false)?;
    fixture.execution.deadline = Some(TokioInstant::now());
    handle_executor_timer(
        &mut fixture.client,
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await?;
    Ok(())
}

fn in_the_future(deadline: Option<TokioInstant>) -> bool {
    deadline.is_some_and(|deadline| deadline > TokioInstant::now())
}

/// The broker's readiness clock runs while job control holds the child, so
/// pausing keeps the supervisor's readiness deadline as it was.
#[tokio::test]
async fn pausing_keeps_the_readiness_deadline() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    let deadline = Some(TokioInstant::now() + HOOK_TIMEOUT);
    fixture.execution.deadline = deadline;

    fixture.dispatch(stopped(generation)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Paused {
            generation,
            ready: false
        }
    );
    assert_eq!(fixture.execution.deadline, deadline);
    Ok(())
}

/// Continuing does not restart the readiness wait: paused time counted.
#[tokio::test]
async fn continuing_keeps_the_original_readiness_deadline() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    let deadline = Some(TokioInstant::now() + HOOK_TIMEOUT);
    fixture.execution.deadline = deadline;
    fixture.dispatch(stopped(generation)?).await?;

    fixture.dispatch(continued(generation)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Running(generation)
    );
    assert_eq!(fixture.execution.deadline, deadline);
    Ok(())
}

/// A resumed generation which already declared readiness needs no deadline.
#[tokio::test]
async fn continuing_a_ready_generation_arms_no_deadline() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    fixture.dispatch(stopped(generation)?).await?;
    fixture
        .dispatch(ProcessBrokerEvent::GenerationReady { generation })
        .await?;

    fixture.dispatch(continued(generation)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Ready(generation)
    );
    assert!(fixture.execution.deadline.is_none());
    Ok(())
}

/// Regression: pausing a service that had not declared readiness for longer
/// than its readiness timeout, then continuing it, ended supervision. The
/// broker's failure arrived during the pause and was only recorded; continuing
/// re-armed a deadline nothing could meet, and its expiry ended the supervisor
/// seven seconds later on a live host.
#[tokio::test]
async fn a_readiness_failure_while_paused_stops_the_service_when_it_continues()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    fixture.execution.deadline = Some(TokioInstant::now() + HOOK_TIMEOUT);
    fixture.dispatch(stopped(generation)?).await?;

    fixture.dispatch(readiness_failed(generation)).await?;
    assert!(fixture.execution.status.readiness_failed);
    assert!(
        fixture.execution.deadline.is_none(),
        "a paused generation whose readiness failed waits for nothing"
    );

    fixture.dispatch(continued(generation)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Stopping(generation)
    );
    assert_eq!(fixture.execution.pending_signals.len(), 2, "TERM and CONT");
    assert!(in_the_future(fixture.execution.deadline));
    Ok(())
}

/// A paused generation still awaiting readiness keeps waiting for the
/// broker's outcome when its deadline passes, and never faults.
#[tokio::test]
async fn an_overdue_readiness_answer_while_paused_keeps_waiting() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    fixture.dispatch(stopped(generation)?).await?;

    fire(&mut fixture).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Paused {
            generation,
            ready: false
        }
    );
    assert!(in_the_future(fixture.execution.deadline));
    Ok(())
}

/// A paused generation whose readiness is settled waits for nothing.
#[tokio::test]
async fn a_paused_generation_with_settled_readiness_waits_for_nothing() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    fixture.dispatch(stopped(generation)?).await?;
    fixture.execution.status.readiness_failed = true;

    fire(&mut fixture).await?;

    assert!(fixture.execution.deadline.is_none());
    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Paused {
            generation,
            ready: false
        }
    );
    Ok(())
}

/// A failure recorded on a running generation is acted on at its deadline.
#[tokio::test]
async fn a_recorded_readiness_failure_is_acted_on_when_its_deadline_expires()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;
    fixture.execution.status.readiness_failed = true;

    fire(&mut fixture).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Stopping(generation)
    );
    Ok(())
}

/// A running generation whose readiness answer is late keeps waiting.
#[tokio::test]
async fn an_overdue_readiness_answer_keeps_waiting() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.notify_generation()?;

    fire(&mut fixture).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Running(generation)
    );
    assert!(in_the_future(fixture.execution.deadline));
    Ok(())
}

/// Regression: a readiness failure arriving while a reload hook ran tried to
/// start the stop hook beside it, which failed as already running and ended
/// supervision.
#[tokio::test]
async fn a_readiness_failure_during_a_reload_hook_waits_for_the_hook() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    let task = BrokerTaskId::new(90).ok_or("invalid test task")?;
    fixture.execution.lifecycle = Some(reload_hook(task, LifecycleHookState::Running));

    fixture.dispatch(readiness_failed(generation)).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Running(generation)
    );
    assert!(fixture.execution.status.readiness_failed);
    assert_eq!(
        fixture.execution.lifecycle.as_ref().map(|hook| hook.kind),
        Some(LifecycleHookKind::Reload)
    );
    Ok(())
}

/// Once the reload hook ends, the recorded failure stops the generation
/// through the stop hook.
#[tokio::test]
async fn a_reload_hook_that_ends_acts_on_a_recorded_readiness_failure() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    let task = BrokerTaskId::new(91).ok_or("invalid test task")?;
    fixture.execution.lifecycle = Some(reload_hook(task, LifecycleHookState::Running));
    fixture.dispatch(readiness_failed(generation)).await?;

    fixture.dispatch(hook_exited(task, 0)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Stopping(generation)
    );
    assert_eq!(
        fixture.execution.lifecycle.as_ref().map(|hook| hook.kind),
        Some(LifecycleHookKind::Stop)
    );
    Ok(())
}

/// Regression: a stop hook that failed to stop a generation whose readiness
/// failed restored it as running with no deadline, and nothing ever tried
/// again: the service stayed running, never ready, for ever.
#[tokio::test]
async fn a_failed_readiness_stop_hook_is_retried_after_the_stop_grace() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    fixture.dispatch(readiness_failed(generation)).await?;
    let task = fixture
        .execution
        .lifecycle
        .as_ref()
        .map(|hook| hook.task)
        .ok_or("the readiness failure started no stop hook")?;
    fixture.dispatch(hook_exited(task, 1)?).await?;
    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Running(generation)
    );
    let retry = fixture.execution.deadline.ok_or("no retry was scheduled")?;
    assert!(retry > TokioInstant::now());
    assert!(retry <= TokioInstant::now() + SERVICE_STOP_GRACE);

    fire(&mut fixture).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Stopping(generation)
    );
    assert!(
        fixture
            .execution
            .lifecycle
            .as_ref()
            .is_some_and(|hook| hook.kind == LifecycleHookKind::Stop && hook.task != task)
    );
    Ok(())
}

/// A shutdown requested while the reload hook ran is carried out by the event
/// loop once the hook ends; the recorded failure starts no stop of its own.
#[tokio::test]
async fn a_pending_shutdown_takes_precedence_over_a_recorded_readiness_failure()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    let task = BrokerTaskId::new(92).ok_or("invalid test task")?;
    fixture.execution.lifecycle = Some(reload_hook(task, LifecycleHookState::Running));
    fixture.dispatch(readiness_failed(generation)).await?;
    fixture.execution.shutdown_requested = true;

    fixture.dispatch(hook_exited(task, 0)?).await?;

    assert!(fixture.execution.lifecycle.is_none());
    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Running(generation)
    );
    Ok(())
}

/// Regression: readiness declared while a descriptor stop hook ran was dropped,
/// and when the hook then failed the live generation came back running, waiting
/// for an outcome the broker never sends twice.
#[tokio::test]
async fn readiness_declared_while_a_stop_hook_runs_survives_its_failure()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    fixture.execution.machine.begin_stop(generation)?;
    let task = BrokerTaskId::new(93).ok_or("invalid test task")?;
    fixture.execution.lifecycle = Some(stop_hook(task, LifecycleHookState::Running));

    fixture
        .dispatch(ProcessBrokerEvent::GenerationReady { generation })
        .await?;
    fixture.dispatch(hook_exited(task, 1)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Ready(generation)
    );
    assert!(fixture.execution.deadline.is_none());
    Ok(())
}

/// Regression: a lifetime that closed while the stop hook ran skipped the group
/// stop, and the hook's success then waited the whole lifetime timeout for a
/// closure that had already happened, failed the stop, and left the launcher
/// running. The launcher is now stopped as soon as the hook succeeds.
#[tokio::test]
async fn a_lifetime_closed_during_a_stop_hook_stops_the_launcher_after_the_hook()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    fixture.execution.machine.child_ready(generation)?;
    fixture.execution.machine.begin_stop(generation)?;
    let task = BrokerTaskId::new(94).ok_or("invalid test task")?;
    fixture.execution.lifecycle = Some(stop_hook(task, LifecycleHookState::Running));

    fixture
        .dispatch(ProcessBrokerEvent::LifetimeClosed { generation })
        .await?;
    assert!(fixture.execution.pending_signals.is_empty());
    fixture.dispatch(hook_exited(task, 0)?).await?;

    assert_eq!(fixture.execution.pending_signals.len(), 2, "TERM and CONT");
    assert!(in_the_future(fixture.execution.deadline));
    let hook = fixture.execution.lifecycle.as_ref().ok_or("hook ended")?;
    assert_eq!(hook.state, LifecycleHookState::WaitingLifetime);
    assert!(
        hook.deadline.is_none(),
        "no wait for a closure that happened"
    );
    Ok(())
}

/// Regression: when the stop hook's success arrived before the lifetime
/// closure, the closure was taken for one during a running hook, the group stop
/// was skipped, and the launcher was left running until the lifetime wait
/// failed the stop. Either order now stops the launcher.
#[tokio::test]
async fn a_lifetime_closed_after_a_stop_hook_succeeded_stops_the_launcher()
-> Result<(), Box<dyn Error>> {
    for closure in [
        ProcessBrokerEvent::LifetimeClosed {
            generation: Generation::FIRST,
        },
        ProcessBrokerEvent::LifetimeFailed {
            generation: Generation::FIRST,
        },
    ] {
        let mut fixture = Fixture::new()?;
        let generation = fixture.descriptor_generation()?;
        fixture.execution.machine.child_ready(generation)?;
        fixture.execution.machine.begin_stop(generation)?;
        let task = BrokerTaskId::new(95).ok_or("invalid test task")?;
        fixture.execution.lifecycle = Some(stop_hook(task, LifecycleHookState::Running));

        fixture.dispatch(hook_exited(task, 0)?).await?;
        let waiting = fixture.execution.lifecycle.as_ref().ok_or("hook ended")?;
        assert_eq!(waiting.state, LifecycleHookState::WaitingLifetime);
        assert!(fixture.execution.pending_signals.is_empty());

        fixture.dispatch(closure.clone()).await?;

        assert_eq!(
            fixture.execution.pending_signals.len(),
            2,
            "TERM and CONT after {closure:?}"
        );
        assert!(in_the_future(fixture.execution.deadline));
        let hook = fixture.execution.lifecycle.as_ref().ok_or("hook ended")?;
        assert!(
            hook.deadline.is_none(),
            "no wait for a closure that happened"
        );
    }
    Ok(())
}

/// Regression: a lifetime that closed while the stop hook ran, followed by the
/// hook's failure, restored a live generation whose service was gone and left
/// its launcher running. The launcher is stopped as after a successful hook.
#[tokio::test]
async fn a_failed_stop_hook_after_the_lifetime_closed_stops_the_launcher()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    fixture.execution.machine.child_ready(generation)?;
    fixture.execution.machine.begin_stop(generation)?;
    let task = BrokerTaskId::new(96).ok_or("invalid test task")?;
    fixture.execution.lifecycle = Some(stop_hook(task, LifecycleHookState::Running));
    fixture
        .dispatch(ProcessBrokerEvent::LifetimeClosed { generation })
        .await?;

    fixture.dispatch(hook_exited(task, 1)?).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Stopping(generation),
        "no live generation is restored once its lifetime closed"
    );
    assert_eq!(fixture.execution.pending_signals.len(), 2, "TERM and CONT");
    assert!(in_the_future(fixture.execution.deadline));
    Ok(())
}

/// Regression: when both the launcher's exit and the lifetime closure arrived
/// while the stop hook ran and the hook then failed, the stop was reverted
/// before the generation completed, so a service an operator stopped was
/// started again. The generation now completes as the stop asked.
#[tokio::test]
async fn a_failed_stop_hook_after_the_service_ended_keeps_the_stop() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = fixture.descriptor_generation()?;
    fixture.execution.machine.child_ready(generation)?;
    fixture.execution.machine.set_desired(DesiredState::Down);
    fixture.execution.machine.begin_stop(generation)?;
    let task = BrokerTaskId::new(97).ok_or("invalid test task")?;
    let mut hook = stop_hook(task, LifecycleHookState::Running);
    hook.after = Some(StopCompletion::Down);
    hook.previous_desired = DesiredState::Up;
    fixture.execution.lifecycle = Some(hook);
    fixture
        .dispatch(ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Exited {
                pid: process()?,
                code: 0,
            },
        })
        .await?;
    fixture
        .dispatch(ProcessBrokerEvent::LifetimeClosed { generation })
        .await?;

    fixture.dispatch(hook_exited(task, 1)?).await?;

    assert_eq!(fixture.execution.machine.state(), SupervisorState::Down);
    assert_eq!(fixture.execution.machine.desired(), DesiredState::Down);
    Ok(())
}
