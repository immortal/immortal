//! One fresh process proving logger restart preserves buffered service output.

#[path = "support/marker.rs"]
mod marker;

use std::{error::Error, fs, io};

use immortal_core::{
    config::{RestartPolicy, ServiceConfig},
    executor::run_foreground,
    supervisor::{ChildResult, SupervisorState},
};

use marker::Marker;

fn main() -> Result<(), Box<dyn Error>> {
    let crashed = Marker::new("logger-restart-crashed");
    let output = Marker::new("logger-restart-output");
    let crashed_path = crashed.path_string()?;
    let output_path = output.path_string()?;
    let service = format!(
        "while [ ! -e '{crashed_path}' ]; do sleep 0.01; done; \
         printf 'stable-output\\n'; \
         while [ ! -e '{output_path}' ]; do sleep 0.01; done"
    );
    let logger = format!(
        "if [ ! -e '{crashed_path}' ]; then : > '{crashed_path}'; exit 23; fi; \
         IFS= read -r line || exit 24; printf '%s' \"$line\" > '{output_path}'; exec sleep 30"
    );
    let mut config =
        ServiceConfig::for_command(vec!["/bin/sh".to_owned(), "-c".to_owned(), service])?;
    config.logging.logger = Some(vec!["/bin/sh".to_owned(), "-c".to_owned(), logger]);
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;

    let outcome = run_foreground(&config)?;
    if outcome.state != SupervisorState::Exited
        || outcome.last_result != Some(ChildResult::Exited(0))
        || outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.starts != 1
    {
        return Err(io::Error::other(format!(
            "unexpected logger-restart supervision outcome: {outcome:?}"
        ))
        .into());
    }
    if !crashed.exists() || !output.exists() {
        return Err(io::Error::other("logger restart markers are missing").into());
    }
    let actual = fs::read_to_string(output_path)?;
    if actual != "stable-output" {
        return Err(io::Error::other(format!("unexpected logger output: {actual}")).into());
    }
    Ok(())
}
