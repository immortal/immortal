//! Tests for what a completed generation leaves the supervisor wanting.

use std::error::Error;

use super::super::{
    DesiredState, SupervisorState, apply_control_command,
    events::handle_broker_event,
    prepare::prepare_execution,
    test_support::{Fixture, process},
};
use crate::{
    config::parse_str,
    control::{
        ControlCommand, GenerationMatch, Operation, Request, ResponseCode, SignalScope,
        StopCompletion,
    },
    process::{BrokerTaskId, ChildEvent, ProcessBrokerEvent, ProcessGroupId},
    supervisor::Generation,
};

/// Bring the fixture's service to Ready under `config` and return its generation.
fn ready_under(fixture: &mut Fixture, config: &str) -> Result<Generation, Box<dyn Error>> {
    fixture.config = parse_str(config)?;
    fixture.execution.machine.set_desired(DesiredState::Up);
    fixture.execution.machine.begin_start()?;
    let generation = fixture.execution.machine.preconditions_ready()?;
    fixture.execution.machine.child_started(generation)?;
    fixture.execution.machine.child_ready(generation)?;
    Ok(generation)
}

/// Deliver the start of the post-exit hook `task` as the event loop does.
async fn hook_started(fixture: &mut Fixture, task: BrokerTaskId) -> Result<(), Box<dyn Error>> {
    let prepared = prepare_execution(&fixture.config, true)?;
    handle_broker_event(
        &mut fixture.client,
        ProcessBrokerEvent::TaskStarted {
            task,
            process: process()?,
            group: ProcessGroupId::try_from(1)?,
        },
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await?;
    Ok(())
}

/// Deliver the end of the post-exit hook `task` as the event loop does.
async fn hook_exited(fixture: &mut Fixture, task: BrokerTaskId) -> Result<(), Box<dyn Error>> {
    let prepared = prepare_execution(&fixture.config, true)?;
    handle_broker_event(
        &mut fixture.client,
        ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code: 0,
            },
        },
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await?;
    Ok(())
}

/// Deliver the main child's exit as the event loop of a controlled supervisor
/// does. Only a controlled supervisor may keep a service down without exiting.
async fn exit(
    fixture: &mut Fixture,
    generation: Generation,
    code: u8,
) -> Result<(), Box<dyn Error>> {
    let prepared = prepare_execution(&fixture.config, true)?;
    handle_broker_event(
        &mut fixture.client,
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Exited {
                pid: process()?,
                code,
            },
        },
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await?;
    Ok(())
}

/// Regression: a service whose restart policy declined a restart was started
/// again at once, without backoff, for ever, because the supervisor still
/// wanted it up: `policy: never` restarted twelve times in six seconds.
#[tokio::test]
async fn a_service_its_policy_does_not_restart_stays_down() -> Result<(), Box<dyn Error>> {
    for (config, code) in [
        (
            "version: 2\ncommand: [/bin/true]\nrestart:\n  policy: never\n",
            1,
        ),
        (
            "version: 2\ncommand: [/bin/true]\nrestart:\n  policy: on-failure\n",
            0,
        ),
    ] {
        let mut fixture = Fixture::new()?;
        let generation = ready_under(&mut fixture, config)?;

        exit(&mut fixture, generation, code).await?;

        assert_eq!(
            fixture.execution.machine.state(),
            SupervisorState::Down,
            "{config}"
        );
        assert_eq!(
            fixture.execution.machine.desired(),
            DesiredState::Down,
            "{config}"
        );
        assert!(fixture.execution.deadline.is_none());
    }
    Ok(())
}

/// An operator restart still starts the service again: only the policy's own
/// decision to stay down changes what the supervisor wants.
#[tokio::test]
async fn an_operator_restart_still_wants_the_service_up() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let generation = ready_under(
        &mut fixture,
        "version: 2\ncommand: [/bin/true]\nrestart:\n  policy: never\n",
    )?;
    fixture.execution.machine.begin_stop(generation)?;
    fixture.execution.pending_stop = Some(StopCompletion::Restart);

    exit(&mut fixture, generation, 0).await?;

    assert_eq!(fixture.execution.machine.state(), SupervisorState::Down);
    assert_eq!(fixture.execution.machine.desired(), DesiredState::Up);
    Ok(())
}

/// Send an operator request through the executor's control handling.
async fn operate(
    fixture: &mut Fixture,
    operation: Operation,
    generation: Generation,
) -> Result<(), Box<dyn Error>> {
    let prepared = prepare_execution(&fixture.config, true)?;
    let (command, response) = ControlCommand::for_test(Request {
        operation,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Exact(generation),
        scope: SignalScope::Main,
        signal: None,
    });
    apply_control_command(
        &mut fixture.client,
        "api",
        command,
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await?;
    let response = response.await?;
    if response.code != ResponseCode::Ok {
        return Err(format!("{operation:?} was refused: {}", response.message).into());
    }
    Ok(())
}

/// Regression: a start, restart, or once an operator sent while the post-exit
/// hook of a service its policy does not restart was still running was
/// overwritten when the hook ended: the policy's older decision set the goal to
/// down, and the requested generation never started.
#[tokio::test]
async fn an_operator_start_during_a_post_exit_hook_outlives_the_policy()
-> Result<(), Box<dyn Error>> {
    for (operation, wanted) in [
        (Operation::Start, DesiredState::Up),
        (Operation::Restart, DesiredState::Up),
        (Operation::Once, DesiredState::Once),
    ] {
        let mut fixture = Fixture::new()?;
        let generation = ready_under(
            &mut fixture,
            "version: 2\ncommand: [/bin/true]\nrestart:\n  policy: never\npost_exit:\n  \
             command: [/bin/true]\n  timeout_seconds: 10\n",
        )?;
        exit(&mut fixture, generation, 1).await?;
        assert_eq!(
            fixture.execution.machine.state(),
            SupervisorState::Completed(generation)
        );
        let task = fixture
            .execution
            .auxiliary
            .task()
            .ok_or("no post-exit hook started")?;
        hook_started(&mut fixture, task).await?;

        operate(&mut fixture, operation, generation).await?;
        hook_exited(&mut fixture, task).await?;

        assert_eq!(fixture.execution.machine.state(), SupervisorState::Down);
        assert_eq!(fixture.execution.machine.desired(), wanted, "{operation:?}");
    }
    Ok(())
}
