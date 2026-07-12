//! One fresh process proving readiness failure reaches the post-exit context.

#[path = "support/marker.rs"]
mod marker;
#[path = "support/post_exit.rs"]
mod post_exit;

use std::{error::Error, fs, io};

use immortal_core::{config::ReadinessMode, executor::run_foreground, supervisor::ChildResult};

use marker::Marker;
use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let context = Marker::new("post-exit-readiness-failure-context");
    let context_path = context.path_string()?;
    let hook = format!(
        "printf '%s:%s:%s:%s' \"$IMMORTAL_EXIT_KIND\" \"$IMMORTAL_EXIT_STATUS\" \
         \"$IMMORTAL_START_FAILED\" \"$IMMORTAL_READINESS_FAILED\" > '{context_path}'"
    );
    let mut config = config(
        "exec sleep 5".to_owned(),
        vec!["/bin/sh".to_owned(), "-c".to_owned(), hook],
        2,
    )?;
    config.readiness.mode = ReadinessMode::NotifyFd;
    config.readiness.timeout_seconds = 1;
    let signal = u8::try_from(libc::SIGTERM)?;

    assert_outcome(
        run_foreground(&config)?,
        ChildResult::Signaled(signal),
        false,
        true,
    )?;
    if !context.exists() {
        return Err(io::Error::other("post-exit readiness-failure marker is missing").into());
    }
    let actual = fs::read_to_string(context_path)?;
    let expected = format!("signal:{signal}:0:1");
    if actual != expected {
        return Err(io::Error::other(format!("unexpected readiness context: {actual}")).into());
    }
    Ok(())
}
