//! One fresh process proving shutdown cancels and reaps an active condition.

#[path = "support/marker.rs"]
mod marker;
mod support;

use std::{error::Error, io};

use immortal_core::{executor::run_foreground, supervisor::SupervisorState};

use marker::Marker;
use support::service_config;

fn main() -> Result<(), Box<dyn Error>> {
    let service = Marker::new("shutdown-service");
    let mut config = service_config(
        &service,
        "kill -TERM \"$SUPERVISOR_PID\"; exec /bin/sleep 5".to_owned(),
    )?;
    config
        .environment
        .insert("SUPERVISOR_PID".to_owned(), std::process::id().to_string());

    let outcome = run_foreground(&config)?;
    if outcome.state != SupervisorState::Exited
        || outcome.starts != 0
        || outcome.last_result.is_some()
        || service.exists()
    {
        return Err(io::Error::other(format!(
            "unexpected condition shutdown outcome: {outcome:?}"
        ))
        .into());
    }
    Ok(())
}
