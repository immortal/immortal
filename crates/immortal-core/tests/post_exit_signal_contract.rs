//! One fresh process proving signal termination reaches the post-exit context.

#[path = "support/marker.rs"]
mod marker;
#[path = "support/post_exit.rs"]
mod post_exit;

use std::{error::Error, fs, io};

use immortal_core::{executor::run_foreground, supervisor::ChildResult};

use marker::Marker;
use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let context = Marker::new("post-exit-signal-context");
    let context_path = context.path_string()?;
    let hook = format!(
        "printf '%s:%s:%s:%s' \"$IMMORTAL_EXIT_KIND\" \"$IMMORTAL_EXIT_STATUS\" \
         \"$IMMORTAL_START_FAILED\" \"$IMMORTAL_READINESS_FAILED\" > '{context_path}'"
    );
    let config = config(
        "kill -TERM $$".to_owned(),
        vec!["/bin/sh".to_owned(), "-c".to_owned(), hook],
        2,
    )?;
    let signal = u8::try_from(libc::SIGTERM)?;

    assert_outcome(
        run_foreground(&config)?,
        ChildResult::Signaled(signal),
        false,
        false,
    )?;
    if !context.exists() {
        return Err(io::Error::other("post-exit signal context marker is missing").into());
    }
    let actual = fs::read_to_string(context_path)?;
    let expected = format!("signal:{signal}:0:0");
    if actual != expected {
        return Err(io::Error::other(format!("unexpected signal context: {actual}")).into());
    }
    Ok(())
}
