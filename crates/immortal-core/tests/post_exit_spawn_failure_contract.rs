//! One fresh process proving hook exec failure preserves the service result.

#[path = "support/post_exit.rs"]
mod post_exit;

use std::error::Error;

use immortal_core::{executor::run_foreground, supervisor::ChildResult};

use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let config = config(
        "exit 0".to_owned(),
        vec!["/definitely/not/an/immortal-post-exit-hook".to_owned()],
        2,
    )?;
    assert_outcome(
        run_foreground(&config)?,
        ChildResult::Exited(0),
        false,
        false,
    )
}
