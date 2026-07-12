//! One fresh process proving timed-out conditions are killed and reaped before retry.

#[path = "support/marker.rs"]
mod marker;
#[path = "support/success.rs"]
mod success;
mod support;

use std::error::Error;

use immortal_core::executor::run_foreground;

use marker::Marker;
use success::assert_success;
use support::service_config;

fn main() -> Result<(), Box<dyn Error>> {
    let condition = Marker::new("timeout");
    let service = Marker::new("timeout-service");
    let condition_path = condition.path_string()?;
    let config = service_config(
        &service,
        format!(
            "if [ -e '{condition_path}' ]; then exit 0; else : > '{condition_path}'; exec /bin/sleep 5; fi"
        ),
    )?;

    assert_success(run_foreground(&config)?, &service)
}
