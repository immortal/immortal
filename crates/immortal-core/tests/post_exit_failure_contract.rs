//! One fresh process proving hook failure does not replace the service result.

#[path = "support/post_exit.rs"]
mod post_exit;

use std::error::Error;

use immortal_core::{executor::run_foreground, supervisor::ChildResult};

use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let config = config(
        "exit 0".to_owned(),
        vec!["/bin/sh".to_owned(), "-c".to_owned(), "exit 23".to_owned()],
        2,
    )?;
    assert_outcome(
        run_foreground(&config)?,
        ChildResult::Exited(0),
        false,
        false,
    )
}
