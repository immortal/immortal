//! One fresh process proving a service waits for a resource that appears late.
//!
//! This is the issue 68 contract: the condition models a network that DHCP
//! configures only during the third check. The failed checks before it must not
//! start the service, and its single start must observe the resource.

#[path = "support/marker.rs"]
mod marker;
#[path = "support/success.rs"]
mod success;
mod support;

use std::{error::Error, fs, io};

use immortal_core::{config, executor::run_foreground};

use marker::Marker;
use success::assert_success;
use support::service_config;

/// The check which first finds the resource; every earlier check fails.
const READY_ATTEMPT: u32 = 3;

fn main() -> Result<(), Box<dyn Error>> {
    let attempts = Marker::new("wait-attempts");
    let resource = Marker::new("wait-resource");
    let service = Marker::new("wait-service");
    let attempts_path = attempts.path_string()?;
    let resource_path = resource.path_string()?;
    let mut config = service_config(
        &service,
        format!(
            "attempt=$(($(cat '{attempts_path}' 2>/dev/null || echo 0) + 1)); \
             echo \"$attempt\" > '{attempts_path}'; \
             if [ \"$attempt\" -ge {READY_ATTEMPT} ]; then : > '{resource_path}'; fi; \
             test -e '{resource_path}'"
        ),
    )?;
    config.command = vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(
            "test -e '{resource_path}' && : > '{}'",
            service.path_string()?
        ),
    ];
    config::validate_service(&config)?;

    assert_success(run_foreground(&config)?, &service)?;
    let observed: u32 = fs::read_to_string(&attempts_path)?.trim().parse()?;
    if observed != READY_ATTEMPT {
        return Err(io::Error::other(format!(
            "expected {READY_ATTEMPT} condition checks before the start, observed {observed}"
        ))
        .into());
    }
    Ok(())
}
