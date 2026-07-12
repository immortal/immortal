//! One fresh process proving exec failure reaches the post-exit context.

#[path = "support/marker.rs"]
mod marker;
#[path = "support/post_exit.rs"]
mod post_exit;

use std::{error::Error, fs, io};

use immortal_core::{executor::run_foreground, supervisor::ChildResult};

use marker::Marker;
use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let context = Marker::new("post-exit-start-failure-context");
    let context_path = context.path_string()?;
    let hook = format!(
        "printf '%s:%s:%s:%s' \"$IMMORTAL_EXIT_KIND\" \"$IMMORTAL_EXIT_STATUS\" \
         \"$IMMORTAL_START_FAILED\" \"$IMMORTAL_READINESS_FAILED\" > '{context_path}'"
    );
    let mut config = config(
        "exit 0".to_owned(),
        vec!["/bin/sh".to_owned(), "-c".to_owned(), hook],
        2,
    )?;
    config.command = vec!["/definitely/not/an/immortal-service".to_owned()];

    let outcome = run_foreground(&config)?;
    assert_outcome(outcome, ChildResult::Exited(127), true, false)?;
    if !context.exists() {
        return Err(io::Error::other("post-exit start-failure marker is missing").into());
    }
    let actual = fs::read_to_string(context_path)?;
    if actual != "exit:127:1:0" {
        return Err(io::Error::other(format!("unexpected start-failure context: {actual}")).into());
    }
    Ok(())
}
