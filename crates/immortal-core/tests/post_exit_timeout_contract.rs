//! One fresh process proving timed-out post-exit groups are killed and reaped.

#[path = "support/marker.rs"]
mod marker;
#[path = "support/post_exit.rs"]
mod post_exit;

use std::{error::Error, io};

use immortal_core::{executor::run_foreground, supervisor::ChildResult};

use marker::Marker;
use post_exit::{assert_outcome, config};

fn main() -> Result<(), Box<dyn Error>> {
    let marker = Marker::new("post-exit-timeout");
    let marker_path = marker.path_string()?;
    let config = config(
        "exit 0".to_owned(),
        vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            format!(": > '{marker_path}'; exec /bin/sleep 5"),
        ],
        1,
    )?;
    assert_outcome(
        run_foreground(&config)?,
        ChildResult::Exited(0),
        false,
        false,
    )?;
    if !marker.exists() {
        return Err(io::Error::other("post-exit hook did not begin before its timeout").into());
    }
    Ok(())
}
