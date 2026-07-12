use std::{error::Error, io};

use immortal_core::{
    config::{CommandHook, RestartPolicy, ServiceConfig},
    executor::SupervisionOutcome,
    supervisor::{ChildResult, SupervisorState},
};

pub fn config(
    service_script: String,
    hook_command: Vec<String>,
    timeout_seconds: u64,
) -> Result<ServiceConfig, Box<dyn Error>> {
    configure(
        vec!["/bin/sh".to_owned(), "-c".to_owned(), service_script],
        hook_command,
        timeout_seconds,
    )
}

fn configure(
    service_command: Vec<String>,
    hook_command: Vec<String>,
    timeout_seconds: u64,
) -> Result<ServiceConfig, Box<dyn Error>> {
    let mut config = ServiceConfig::for_command(service_command)?;
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;
    config.post_exit = Some(CommandHook {
        command: hook_command,
        timeout_seconds,
    });
    Ok(config)
}

pub fn assert_outcome(
    outcome: SupervisionOutcome,
    expected: ChildResult,
    start_failed: bool,
    readiness_failed: bool,
) -> Result<(), Box<dyn Error>> {
    if outcome.state != SupervisorState::Exited
        || outcome.last_result != Some(expected)
        || outcome.last_start_failed != start_failed
        || outcome.last_readiness_failed != readiness_failed
        || outcome.starts != 1
    {
        return Err(io::Error::other(format!(
            "unexpected post-exit supervision outcome: {outcome:?}"
        ))
        .into());
    }
    Ok(())
}
