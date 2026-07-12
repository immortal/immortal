//! One fresh process proving logical lifetime survives the daemonizing launcher.

use std::{
    error::Error,
    io,
    time::{Duration, Instant},
};

use immortal_core::{
    config::{CommandHook, DescriptorTrackingConfig, ProcessMode, RestartPolicy, ServiceConfig},
    executor::run_foreground,
    supervisor::{ChildResult, SupervisorState},
};

const TRUE_PROGRAM: &str = "/usr/bin/true";

fn main() -> Result<(), Box<dyn Error>> {
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "(/bin/sleep 0.2) & exit 0".to_owned(),
    ])?;
    config.process_mode = ProcessMode::DescriptorTracking;
    config.descriptor_tracking = Some(DescriptorTrackingConfig {
        stop: hook(TRUE_PROGRAM),
        reload: hook(TRUE_PROGRAM),
        lifetime_timeout_seconds: 1,
    });
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;

    let started = Instant::now();
    let outcome = run_foreground(&config)?;
    if started.elapsed() < Duration::from_millis(150) {
        return Err(io::Error::other(
            "descriptor generation ended before its inherited lifetime closed",
        )
        .into());
    }
    if outcome.state != SupervisorState::Exited
        || outcome.last_result != Some(ChildResult::LifetimeClosed)
        || outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.starts != 1
    {
        return Err(io::Error::other(format!(
            "unexpected descriptor supervision outcome: {outcome:?}"
        ))
        .into());
    }
    Ok(())
}

fn hook(program: &str) -> CommandHook {
    CommandHook {
        command: vec![program.to_owned()],
        timeout_seconds: 1,
    }
}
