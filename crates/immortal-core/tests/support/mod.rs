use std::error::Error;

use immortal_core::config::{
    ConditionBackoffConfig, RestartPolicy, ServiceConfig, StartConditionConfig,
};

use crate::marker::Marker;

pub fn service_config(
    marker: &Marker,
    condition_script: String,
) -> Result<ServiceConfig, Box<dyn Error>> {
    let mut config = ServiceConfig::for_command(vec![
        "/bin/sh".to_owned(),
        "-c".to_owned(),
        format!(": > '{}'; exit 0", marker.path_string()?),
    ])?;
    config.restart.policy = RestartPolicy::Never;
    config.restart.exit_when_done = true;
    config.start_condition = Some(StartConditionConfig {
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), condition_script],
        timeout_seconds: 1,
        backoff: ConditionBackoffConfig {
            initial_seconds: 1,
            max_seconds: 1,
            multiplier: 1,
            jitter_percent: 0,
        },
    });
    Ok(config)
}
