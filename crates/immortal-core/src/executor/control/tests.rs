//! Tests for the executor's control and shutdown helpers.

use std::error::Error;

use super::super::{
    DesiredState, SupervisorState, prepare::prepare_execution, test_support::Fixture,
};
use super::begin_requested_shutdown;

/// Regression: a shutdown requested while a stop hook ran was carried out
/// as a descriptor shutdown even when that hook had ended the generation, and
/// failed because there was no generation left to stop. It ended supervision
/// with an error where a plain, clean shutdown was due.
#[tokio::test]
async fn a_shutdown_requested_during_a_hook_that_ended_the_generation_is_the_plain_one()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let prepared = prepare_execution(&fixture.config, false)?;
    assert_eq!(fixture.execution.machine.state(), SupervisorState::Down);
    assert_eq!(fixture.execution.machine.desired(), DesiredState::Up);

    begin_requested_shutdown(
        &mut fixture.client,
        &prepared.execution,
        &fixture.config,
        &mut fixture.execution,
    )
    .await?;

    assert_eq!(fixture.execution.machine.desired(), DesiredState::Halt);
    Ok(())
}
