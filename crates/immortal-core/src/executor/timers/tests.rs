//! Tests for timer arms that wait for the broker.
//!
//! The broker bounds every operation these deadlines wait for, so one passes
//! only while the broker is alive but late: starved, paused, or stopped. Each
//! arm must keep waiting with its deadline moved into the future, never end
//! supervision and never spin the event loop.

use std::error::Error;

use tokio::time::Instant as TokioInstant;

use super::super::{
    AuxiliaryExecution, DesiredState, Generation, SupervisorState,
    prepare::prepare_execution,
    test_support::{CONDITION, Fixture, POST_EXIT, reload_hook},
};
use super::{handle_executor_timer, handle_lifecycle_timer};
use crate::{config::parse_str, process::BrokerTaskId, supervisor::RestartDecision};

/// Bring the fixture's service to `Starting` and return its generation.
fn starting(fixture: &mut Fixture) -> Result<Generation, Box<dyn Error>> {
    fixture.execution.machine.set_desired(DesiredState::Up);
    fixture.execution.machine.begin_start()?;
    Ok(fixture.execution.machine.preconditions_ready()?)
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

/// Puts a fixture into the state one overdue arm is tested in.
type Arrange = fn(&mut Fixture, BrokerTaskId) -> Result<(), Box<dyn Error>>;

fn rearmed_in_the_future(deadline: Option<TokioInstant>) -> bool {
    deadline.is_some_and(|deadline| deadline > TokioInstant::now())
}

/// Regression: a start the broker did not confirm within ten seconds ended
/// supervision, although the broker bounds the start itself and was only late.
#[tokio::test]
async fn an_overdue_service_start_keeps_waiting() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = starting(&mut fixture)?;

    fire(&mut fixture).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Starting(generation)
    );
    assert!(rearmed_in_the_future(fixture.execution.deadline));
    Ok(())
}

/// Regression: a kill whose reap the broker had not reported five seconds
/// later ended supervision, and the service with it. A broker stalled for
/// twelve seconds during a restart reproduced it on a live host.
#[tokio::test]
async fn an_overdue_service_kill_keeps_waiting() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = starting(&mut fixture)?;
    fixture.execution.machine.child_started(generation)?;
    fixture.execution.machine.begin_stop(generation)?;
    fixture.execution.stop_kill_sent = true;

    fire(&mut fixture).await?;

    assert_eq!(
        fixture.execution.machine.state(),
        SupervisorState::Stopping(generation),
        "no replacement may start before the kill is confirmed"
    );
    assert!(rearmed_in_the_future(fixture.execution.deadline));
    Ok(())
}

/// Every arm that waits for the broker keeps waiting with a future deadline,
/// so none ends supervision and none can make the event loop spin.
#[tokio::test]
async fn every_overdue_arm_moves_the_deadline_into_the_future() -> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(80).ok_or("invalid test task")?;
    let condition = parse_str(CONDITION)?;
    let post_exit = parse_str(POST_EXIT)?;
    let cases: [(&str, Arrange); 7] = [
        ("service readiness", |fixture, _| {
            let generation = starting(fixture)?;
            fixture.execution.machine.child_started(generation)?;
            Ok(())
        }),
        ("condition start", |fixture, task| {
            fixture.execution.machine.set_desired(DesiredState::Up);
            fixture.execution.machine.begin_start()?;
            fixture.execution.auxiliary = AuxiliaryExecution::Starting(task);
            Ok(())
        }),
        ("condition cleanup", |fixture, task| {
            fixture.execution.machine.set_desired(DesiredState::Up);
            fixture.execution.machine.begin_start()?;
            fixture.execution.auxiliary = AuxiliaryExecution::Killing { task, retry: true };
            Ok(())
        }),
        (
            "condition cleanup after the service went down",
            |fixture, task| {
                fixture.execution.machine.set_desired(DesiredState::Up);
                fixture.execution.machine.begin_start()?;
                fixture.execution.machine.cancel_pending()?;
                fixture.execution.auxiliary = AuxiliaryExecution::Killing { task, retry: false };
                Ok(())
            },
        ),
        ("post-exit start", |fixture, task| {
            let generation = starting(fixture)?;
            fixture.execution.machine.child_completed(generation)?;
            fixture.execution.auxiliary = AuxiliaryExecution::HookStarting(task);
            Ok(())
        }),
        ("post-exit cleanup", |fixture, task| {
            let generation = starting(fixture)?;
            fixture.execution.machine.child_completed(generation)?;
            fixture.execution.auxiliary = AuxiliaryExecution::HookKilling(task);
            Ok(())
        }),
        ("service start", |fixture, _| {
            starting(fixture)?;
            Ok(())
        }),
    ];
    for (name, arrange) in cases {
        let mut fixture = Fixture::new()?;
        fixture.config = if name.starts_with("condition") {
            condition.clone()
        } else {
            post_exit.clone()
        };
        arrange(&mut fixture, task)?;
        let before = fixture.execution.machine.state();

        fire(&mut fixture)
            .await
            .map_err(|error| format!("{name}: {error}"))?;

        assert_eq!(fixture.execution.machine.state(), before, "{name}");
        assert!(rearmed_in_the_future(fixture.execution.deadline), "{name}");
    }
    Ok(())
}

/// A lifecycle hook whose start or cleanup is late keeps waiting too.
#[tokio::test]
async fn every_overdue_lifecycle_arm_keeps_waiting() -> Result<(), Box<dyn Error>> {
    let task = BrokerTaskId::new(81).ok_or("invalid test task")?;
    for state in [
        super::super::LifecycleHookState::Starting,
        super::super::LifecycleHookState::Killing,
    ] {
        let mut fixture = Fixture::new()?;
        let prepared = prepare_execution(&fixture.config, false)?;
        let mut hook = reload_hook(task, state);
        hook.deadline = Some(TokioInstant::now());
        fixture.execution.lifecycle = Some(hook);

        handle_lifecycle_timer(
            &mut fixture.client,
            &prepared.execution,
            &fixture.config,
            &mut fixture.execution,
        )
        .await?;

        let hook = fixture.execution.lifecycle.as_ref().ok_or("hook ended")?;
        assert_eq!(hook.state, state);
        assert!(rearmed_in_the_future(hook.deadline), "{state:?}");
    }
    Ok(())
}

/// Regression: a kill the helper never received left the service running while
/// the supervisor waited for its reap for good. The kill is asked for again
/// while the reap is overdue, but only when no earlier request is unanswered,
/// so a stalled broker's socket never fills with repeats.
#[tokio::test]
async fn an_overdue_service_kill_is_asked_for_again() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = starting(&mut fixture)?;
    fixture.execution.machine.child_started(generation)?;
    fixture.execution.machine.begin_stop(generation)?;
    fixture.execution.stop_kill_sent = true;

    fire(&mut fixture).await?;
    assert_eq!(fixture.execution.pending_signals.len(), 1, "kill repeated");

    fire(&mut fixture).await?;
    assert_eq!(
        fixture.execution.pending_signals.len(),
        1,
        "no repeat while the last request is unanswered"
    );
    Ok(())
}

/// A supervisor told to halt is ending, so its overdue waits stay bounded and
/// its broker teardown cleans up within bounds instead of shutdown hanging.
#[tokio::test]
async fn overdue_waits_stay_bounded_while_the_supervisor_ends() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = starting(&mut fixture)?;
    fixture.execution.machine.child_started(generation)?;
    fixture.execution.machine.set_desired(DesiredState::Halt);
    fixture.execution.machine.begin_stop(generation)?;
    fixture.execution.stop_kill_sent = true;
    let outcome = fire(&mut fixture).await;
    assert!(
        outcome.is_err_and(|error| error.to_string().contains("service termination")),
        "a halting supervisor must not wait for ever"
    );

    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(POST_EXIT)?;
    let generation = starting(&mut fixture)?;
    fixture.execution.machine.child_completed(generation)?;
    fixture.execution.machine.set_desired(DesiredState::Halt);
    fixture
        .execution
        .machine
        .completion_finished(generation, RestartDecision::ExitSupervisor)?;
    let task = BrokerTaskId::new(82).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::HookKilling(task);
    let outcome = fire(&mut fixture).await;
    assert!(outcome.is_err_and(|error| error.to_string().contains("post-exit hook cleanup")));
    Ok(())
}

/// Regression: a TERM that arrived while a descriptor lifecycle hook ran was
/// only queued behind the hook, and a hook whose start or reap the stalled
/// broker never confirmed was waited for indefinitely, so shutdown hung.
#[tokio::test]
async fn a_queued_shutdown_keeps_lifecycle_hook_waits_bounded() -> Result<(), Box<dyn Error>> {
    for state in [
        super::super::LifecycleHookState::Starting,
        super::super::LifecycleHookState::Killing,
    ] {
        let mut fixture = Fixture::new()?;
        let prepared = prepare_execution(&fixture.config, false)?;
        let task = BrokerTaskId::new(83).ok_or("invalid test task")?;
        let mut hook = reload_hook(task, state);
        hook.deadline = Some(TokioInstant::now());
        fixture.execution.lifecycle = Some(hook);
        fixture.execution.shutdown_requested = true;

        let outcome = handle_lifecycle_timer(
            &mut fixture.client,
            &prepared.execution,
            &fixture.config,
            &mut fixture.execution,
        )
        .await;

        assert!(
            outcome.is_err_and(|error| error.to_string().contains("descriptor lifecycle hook")),
            "{state:?}"
        );
    }
    Ok(())
}
