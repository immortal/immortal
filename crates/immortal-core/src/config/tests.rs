//! End-to-end round-trip coverage for the public parse/emit pipeline.
//!
//! [`supported_configuration_round_trips`] is the single black-box contract
//! proving every child module composes correctly: a definition using every
//! documented field parses into the exact typed model built here, and
//! re-emitting then re-parsing that model reproduces it byte-for-byte
//! equivalent. The `complete_*` builders exist only to give this one test an
//! unambiguous expected value without duplicating it inline.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    path::PathBuf,
};

use super::{
    BackoffConfig, CommandHook, ConditionBackoffConfig, DescriptorTrackingConfig, EnvironmentMode,
    FileLogConfig, FileLogRoutes, LoggerRestartConfig, LoggingConfig, PidFiles, ProcessMode,
    ReadinessConfig, ReadinessMode, RestartBurstLimit, RestartConfig, RestartLimits, RestartPolicy,
    ServiceConfig, StartConditionConfig, emit_config, parse_str,
};

#[test]
fn supported_configuration_round_trips() -> Result<(), Box<dyn Error>> {
    let config = parse_str(include_str!("../../tests/fixtures/v2/complete.yml"))?;
    let expected = complete_config();
    assert_eq!(config, expected);

    let emitted = emit_config(&config)?;
    let reparsed = parse_str(&emitted)?;

    assert_eq!(reparsed, config);
    assert!(emitted.contains("version: 2"));
    Ok(())
}

fn complete_config() -> ServiceConfig {
    ServiceConfig {
        enabled: false,
        command: vec!["/usr/local/bin/api".to_owned(), "--foreground".to_owned()],
        working_directory: Some(PathBuf::from("/srv/api")),
        environment: BTreeMap::from([("MODE".to_owned(), "test".to_owned())]),
        environment_mode: EnvironmentMode::Clear,
        user: Some("www".to_owned()),
        start_delay_seconds: 7,
        restart: complete_restart(),
        readiness: ReadinessConfig {
            mode: ReadinessMode::NotifyFd,
            timeout_seconds: 15,
        },
        requires: vec!["database".to_owned(), "network".to_owned()],
        start_condition: Some(complete_condition()),
        post_exit: Some(CommandHook {
            command: vec![
                "/usr/local/libexec/api-cleanup".to_owned(),
                "--quiet".to_owned(),
            ],
            timeout_seconds: 9,
        }),
        logging: complete_logging(),
        pid_files: PidFiles {
            supervisor: Some(PathBuf::from("/var/run/api.supervisor.pid")),
            main: Some(PathBuf::from("/var/run/api.pid")),
        },
        process_mode: ProcessMode::DescriptorTracking,
        descriptor_tracking: Some(complete_descriptor_tracking()),
    }
}

fn complete_restart() -> RestartConfig {
    RestartConfig {
        policy: RestartPolicy::OnFailure,
        success_exit_codes: BTreeSet::from([0, 2]),
        exit_when_done: true,
        limits: RestartLimits {
            max_retries: Some(4),
            max_elapsed_seconds: Some(300),
            burst: Some(RestartBurstLimit {
                starts: 5,
                window_seconds: 60,
            }),
        },
        backoff: BackoffConfig {
            initial_seconds: 2,
            max_seconds: 20,
            multiplier: 3,
            jitter_percent: 10,
            reset_after_seconds: 120,
        },
    }
}

fn complete_condition() -> StartConditionConfig {
    StartConditionConfig {
        command: vec![
            "/usr/bin/test".to_owned(),
            "-e".to_owned(),
            "/run/network-ready".to_owned(),
        ],
        timeout_seconds: 5,
        backoff: ConditionBackoffConfig {
            initial_seconds: 3,
            max_seconds: 30,
            multiplier: 2,
            jitter_percent: 5,
        },
    }
}

fn complete_logging() -> LoggingConfig {
    LoggingConfig {
        files: Some(FileLogRoutes::Selected {
            stdout: Some(complete_file(
                "/var/log/api.log",
                86_400,
                7,
                10_485_760,
                true,
            )),
            stderr: Some(complete_file(
                "/var/log/api.err",
                3_600,
                3,
                1_048_576,
                false,
            )),
        }),
        logger: Some(vec![
            "/usr/bin/logger".to_owned(),
            "-t".to_owned(),
            "api".to_owned(),
        ]),
        file_adapter: Some(PathBuf::from("/usr/local/bin/immortallog")),
        restart: LoggerRestartConfig {
            max_retries: Some(6),
            backoff: BackoffConfig {
                initial_seconds: 4,
                max_seconds: 40,
                multiplier: 2,
                jitter_percent: 15,
                reset_after_seconds: 180,
            },
        },
    }
}

fn complete_file(
    path: &str,
    max_age_seconds: u64,
    keep: u32,
    max_bytes: u64,
    timestamp: bool,
) -> FileLogConfig {
    FileLogConfig {
        file: PathBuf::from(path),
        max_age_seconds: Some(max_age_seconds),
        keep: Some(keep),
        max_bytes: Some(max_bytes),
        timestamp,
    }
}

fn complete_descriptor_tracking() -> DescriptorTrackingConfig {
    DescriptorTrackingConfig {
        stop: CommandHook {
            command: vec!["/usr/local/bin/api-control".to_owned(), "stop".to_owned()],
            timeout_seconds: 11,
        },
        reload: CommandHook {
            command: vec!["/usr/local/bin/api-control".to_owned(), "reload".to_owned()],
            timeout_seconds: 12,
        },
        lifetime_timeout_seconds: 13,
    }
}
