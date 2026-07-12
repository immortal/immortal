//! One fresh process proving shutdown cancels and reaps an active post-exit hook.

#[path = "support/post_exit.rs"]
mod post_exit;

use std::error::Error;

use immortal_core::{executor::run_foreground, supervisor::ChildResult};

use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let mut config = config(
        "exit 0".to_owned(),
        vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "kill -TERM \"$SUPERVISOR_PID\"; exec /bin/sleep 5".to_owned(),
        ],
        4,
    )?;
    config
        .environment
        .insert("SUPERVISOR_PID".to_owned(), std::process::id().to_string());
    assert_outcome(
        run_foreground(&config)?,
        ChildResult::Exited(0),
        false,
        false,
    )
}
