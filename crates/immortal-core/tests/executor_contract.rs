//! One fresh process proving pre-Tokio broker creation and restart execution.

use std::{error::Error, fs, io, path::PathBuf};

use immortal_core::{
    config::{RestartPolicy, ServiceConfig},
    executor::run_foreground,
    supervisor::{ChildResult, SupervisorState},
};

fn main() -> Result<(), Box<dyn Error>> {
    let marker = Marker::new();
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        "if [ -e \"$MARKER\" ]; then exit 0; else : > \"$MARKER\"; exit 1; fi".to_owned(),
    ])?;
    config
        .environment
        .insert("MARKER".to_owned(), marker.path_string()?);
    config.restart.policy = RestartPolicy::OnFailure;
    config.restart.exit_when_done = true;
    config.restart.limits.max_retries = Some(2);
    config.restart.backoff.initial_seconds = 1;
    config.restart.backoff.max_seconds = 1;
    config.restart.backoff.jitter_percent = 0;
    let outcome = run_foreground(&config)?;
    if outcome.state != SupervisorState::Exiting
        || outcome.last_result != Some(ChildResult::Exited(0))
        || outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.starts != 2
    {
        return Err(io::Error::other(format!(
            "unexpected foreground supervision outcome: {outcome:?}"
        ))
        .into());
    }
    Ok(())
}

struct Marker(PathBuf);

impl Marker {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("immortal-executor-restart-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        Self(path)
    }

    fn path_string(&self) -> Result<String, Box<dyn Error>> {
        self.0
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| "restart marker path is not UTF-8".into())
    }
}

impl Drop for Marker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
