use std::{error::Error, io};

use immortal_core::{
    executor::SupervisionOutcome,
    supervisor::{ChildResult, SupervisorState},
};

use crate::marker::Marker;

pub fn assert_success(outcome: SupervisionOutcome, service: &Marker) -> Result<(), Box<dyn Error>> {
    if outcome.state != SupervisorState::Exited
        || outcome.last_result != Some(ChildResult::Exited(0))
        || outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.starts != 1
        || !service.exists()
    {
        return Err(io::Error::other(format!(
            "unexpected condition supervision outcome: {outcome:?}"
        ))
        .into());
    }
    Ok(())
}
