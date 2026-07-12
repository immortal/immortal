//! One fresh process proving logger EOF drain precedes broker shutdown.

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
    let output = Marker::new("logger-drain-output");
    let output_path = output.path_string()?;
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "printf 'drained-output\\n'".to_owned(),
    ])?;
    config.logging.combine_stderr = true;
    config.logging.stdout.logger = Some(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!("cat > '{output_path}'"),
    ]);
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
            "unexpected logger-drain supervision outcome: {outcome:?}"
        ))
        .into());
    }
    if !output.exists() {
        return Err(io::Error::other("drained logger output is missing").into());
    }
    let actual = fs::read_to_string(output_path)?;
    if actual != "drained-output\n" {
        return Err(io::Error::other(format!("unexpected drained output: {actual:?}")).into());
    }
    Ok(())
}
