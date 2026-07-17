//! Policy bounds and cross-field invariants for one service definition.
//!
//! [`validate`] is the single gate applied to both freshly parsed
//! `version: 2` documents and directly constructed
//! [`super::model::ServiceConfig`] values (see
//! [`super::model::ServiceConfig::for_command`] and
//! [`super::model::LoggingConfig::for_direct`]), so every construction path
//! observes identical bounds. Every failure is collected rather than
//! returned on the first violation, so callers see every problem in one
//! [`super::ConfigError::Validation`] report instead of iterating.

use std::{collections::BTreeSet, path::Path};

use crate::service_name::is_safe_service_name;

use super::{
    BackoffConfig, CommandHook, ConfigError, FileLogConfig, FileLogRoutes, LoggerRestartConfig,
    LoggingConfig, ProcessMode, ServiceConfig, StartConditionConfig,
    environment::{environment_key_is_valid, environment_value_is_valid},
};

const MAX_OPERATION_SECONDS: u64 = 86_400;
const MAX_SCHEDULE_SECONDS: u64 = 31_536_000;

pub(super) fn validate(config: &ServiceConfig) -> Result<(), ConfigError> {
    let mut errors = Vec::new();
    validate_argv(&config.command, "command", &mut errors);
    validate_optional_text(config.user.as_deref(), "user", &mut errors);
    for (key, value) in &config.environment {
        if !environment_key_is_valid(key) {
            errors.push(format!("environment key `{key}` is invalid"));
        }
        if !environment_value_is_valid(value) {
            errors.push(format!("environment value for `{key}` contains NUL"));
        }
    }
    validate_service_names(&config.requires, &mut errors);
    if config.start_delay_seconds > MAX_SCHEDULE_SECONDS {
        errors.push(format!(
            "start_delay_seconds must not exceed {MAX_SCHEDULE_SECONDS}"
        ));
    }
    if config.restart.success_exit_codes.is_empty() {
        errors.push("restart.success_exit_codes must not be empty".to_owned());
    }
    validate_backoff(&config.restart.backoff, "restart.backoff", &mut errors);
    if let Some(burst) = &config.restart.limits.burst
        && (burst.starts == 0 || burst.window_seconds == 0)
    {
        errors.push("restart.limits.burst values must be greater than zero".to_owned());
    }
    if config.readiness.timeout_seconds == 0 {
        errors.push("readiness.timeout_seconds must be greater than zero".to_owned());
    } else if config.readiness.timeout_seconds > MAX_OPERATION_SECONDS {
        errors.push(format!(
            "readiness.timeout_seconds must not exceed {MAX_OPERATION_SECONDS}"
        ));
    }
    if let Some(hook) = &config.start_condition {
        validate_start_condition(hook, &mut errors);
    }
    if let Some(hook) = &config.post_exit {
        validate_hook(hook, "post_exit", &mut errors);
    }
    validate_logging(&config.logging, &mut errors);
    validate_path(
        config.working_directory.as_deref(),
        "working_directory",
        &mut errors,
    );
    validate_path(
        config.pid_files.supervisor.as_deref(),
        "pid_files.supervisor",
        &mut errors,
    );
    validate_path(
        config.pid_files.main.as_deref(),
        "pid_files.main",
        &mut errors,
    );

    match (config.process_mode, config.descriptor_tracking.as_ref()) {
        (ProcessMode::DescriptorTracking, Some(tracking)) => {
            validate_hook(&tracking.stop, "descriptor_tracking.stop", &mut errors);
            validate_hook(&tracking.reload, "descriptor_tracking.reload", &mut errors);
            if tracking.lifetime_timeout_seconds == 0 {
                errors.push(
                    "descriptor_tracking.lifetime_timeout_seconds must be greater than zero"
                        .to_owned(),
                );
            } else if tracking.lifetime_timeout_seconds > MAX_OPERATION_SECONDS {
                errors.push(format!(
                    "descriptor_tracking.lifetime_timeout_seconds must not exceed {MAX_OPERATION_SECONDS}"
                ));
            }
        }
        (ProcessMode::DescriptorTracking, None) => errors.push(
            "descriptor-tracking process mode requires descriptor_tracking stop and reload hooks"
                .to_owned(),
        ),
        (ProcessMode::Foreground, Some(_)) => errors.push(
            "descriptor_tracking is valid only with process_mode: descriptor-tracking".to_owned(),
        ),
        (ProcessMode::Foreground, None) => {}
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors))
    }
}

fn validate_logging(config: &LoggingConfig, errors: &mut Vec<String>) {
    if let Some(files) = &config.files {
        match files {
            FileLogRoutes::Combined(file) => validate_file_log(file, "log", errors),
            FileLogRoutes::Selected { stdout, stderr } => {
                if stdout.is_none() && stderr.is_none() {
                    errors.push("log must configure stdout or stderr".to_owned());
                }
                if let Some(file) = stdout {
                    validate_file_log(file, "log.stdout", errors);
                }
                if let Some(file) = stderr {
                    validate_file_log(file, "log.stderr", errors);
                }
            }
        }
    }
    if let Some(logger) = &config.logger {
        validate_argv(logger, "logger", errors);
    }
    validate_backoff(&config.restart.backoff, "logger_restart.backoff", errors);
    if config.logger.is_none() && config.restart != LoggerRestartConfig::default() {
        errors.push("logger_restart requires logger".to_owned());
    }
    validate_path(config.file_adapter.as_deref(), "log_adapter", errors);
    if config.file_adapter.is_some() && config.files.is_none() {
        errors.push("log_adapter requires log".to_owned());
    }
}

fn validate_backoff(backoff: &BackoffConfig, path: &str, errors: &mut Vec<String>) {
    if backoff.initial_seconds == 0 {
        errors.push(format!("{path}.initial_seconds must be greater than zero"));
    }
    if backoff.max_seconds < backoff.initial_seconds {
        errors.push(format!(
            "{path}.max_seconds must be at least initial_seconds"
        ));
    }
    if backoff.max_seconds > MAX_SCHEDULE_SECONDS {
        errors.push(format!(
            "{path}.max_seconds must not exceed {MAX_SCHEDULE_SECONDS}"
        ));
    }
    if backoff.reset_after_seconds > MAX_SCHEDULE_SECONDS {
        errors.push(format!(
            "{path}.reset_after_seconds must not exceed {MAX_SCHEDULE_SECONDS}"
        ));
    }
    if backoff.multiplier == 0 {
        errors.push(format!("{path}.multiplier must be at least one"));
    }
    if backoff.jitter_percent > 100 {
        errors.push(format!("{path}.jitter_percent must not exceed 100"));
    }
}

fn validate_hook(hook: &CommandHook, path: &str, errors: &mut Vec<String>) {
    validate_argv(&hook.command, &format!("{path}.command"), errors);
    if hook.timeout_seconds == 0 {
        errors.push(format!("{path}.timeout_seconds must be greater than zero"));
    } else if hook.timeout_seconds > MAX_OPERATION_SECONDS {
        errors.push(format!(
            "{path}.timeout_seconds must not exceed {MAX_OPERATION_SECONDS}"
        ));
    }
}

fn validate_start_condition(condition: &StartConditionConfig, errors: &mut Vec<String>) {
    validate_argv(&condition.command, "start_condition.command", errors);
    if condition.timeout_seconds == 0 {
        errors.push("start_condition.timeout_seconds must be greater than zero".to_owned());
    } else if condition.timeout_seconds > MAX_OPERATION_SECONDS {
        errors.push(format!(
            "start_condition.timeout_seconds must not exceed {MAX_OPERATION_SECONDS}"
        ));
    }
    let backoff = &condition.backoff;
    if backoff.initial_seconds == 0 {
        errors.push("start_condition.backoff.initial_seconds must be greater than zero".to_owned());
    }
    if backoff.max_seconds < backoff.initial_seconds {
        errors.push(
            "start_condition.backoff.max_seconds must be at least initial_seconds".to_owned(),
        );
    }
    if backoff.max_seconds > MAX_SCHEDULE_SECONDS {
        errors.push(format!(
            "start_condition.backoff.max_seconds must not exceed {MAX_SCHEDULE_SECONDS}"
        ));
    }
    if backoff.multiplier == 0 {
        errors.push("start_condition.backoff.multiplier must be at least one".to_owned());
    }
    if backoff.jitter_percent > 100 {
        errors.push("start_condition.backoff.jitter_percent must not exceed 100".to_owned());
    }
}

pub(super) fn validate_file_log(file: &FileLogConfig, path: &str, errors: &mut Vec<String>) {
    validate_path(Some(&file.file), &format!("{path}.file"), errors);
    if file.max_age_seconds == Some(0) {
        errors.push(format!("{path}.age must be greater than zero"));
    }
    if file.max_bytes == Some(0) {
        errors.push(format!("{path}.size must be greater than zero"));
    }
    if file.keep == Some(0) {
        errors.push(format!("{path}.keep must be greater than zero"));
    }
    if file.keep.is_some() && file.max_age_seconds.is_none() && file.max_bytes.is_none() {
        errors.push(format!("{path}.keep requires age or size"));
    }
}

pub(super) fn validate_argv(argv: &[String], path: &str, errors: &mut Vec<String>) {
    match argv.first() {
        None => errors.push(format!("{path} must contain an executable")),
        Some(executable) if executable.is_empty() => {
            errors.push(format!("{path} executable must not be empty"));
        }
        Some(_) => {}
    }
    if argv.iter().any(|argument| argument.contains('\0')) {
        errors.push(format!("{path} must not contain NUL"));
    }
}

fn validate_optional_text(value: Option<&str>, path: &str, errors: &mut Vec<String>) {
    if value.is_some_and(|text| text.is_empty() || text.contains('\0')) {
        errors.push(format!("{path} must not be empty or contain NUL"));
    }
}

fn validate_service_names(names: &[String], errors: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    for name in names {
        if !is_safe_service_name(name) {
            errors.push(format!(
                "requires entry `{name}` is not a safe service name"
            ));
        } else if !seen.insert(name) {
            errors.push(format!("requires contains duplicate service `{name}`"));
        }
    }
}

fn validate_path(path: Option<&Path>, field: &str, errors: &mut Vec<String>) {
    if path.is_some_and(|value| value.as_os_str().is_empty()) {
        errors.push(format!("{field} must not be empty"));
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use crate::config::{ConfigError, ProcessMode, RestartPolicy, parse_str};

    #[test]
    fn parses_strict_v2_restart_and_readiness_policy() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            r"
version: 2
command: [/usr/bin/example, --foreground]
restart:
  policy: on-failure
  success_exit_codes: [0, 2]
  exit_when_done: true
  limits:
    max_retries: 10
    max_elapsed_seconds: 300
    burst:
      starts: 5
      window_seconds: 60
  backoff:
    initial_seconds: 2
    max_seconds: 30
    multiplier: 2
    jitter_percent: 10
    reset_after_seconds: 120
readiness:
  mode: notify-fd
  timeout_seconds: 15
",
        )?;

        assert_eq!(config.restart.policy, RestartPolicy::OnFailure);
        assert_eq!(config.restart.limits.max_retries, Some(10));
        assert!(config.restart.exit_when_done);
        Ok(())
    }

    #[test]
    fn start_condition_has_independent_typed_backoff() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            r"
version: 2
command: [service]
start_condition:
  command: [/usr/bin/test, -e, /run/network-ready]
  timeout_seconds: 5
  backoff:
    initial_seconds: 2
    max_seconds: 20
    multiplier: 2
    jitter_percent: 5
",
        )?;
        let condition = config
            .start_condition
            .as_ref()
            .ok_or_else(|| io::Error::other("start condition missing"))?;
        assert_eq!(condition.timeout_seconds, 5);
        assert_eq!(condition.backoff.initial_seconds, 2);
        assert_eq!(condition.backoff.max_seconds, 20);

        let invalid = parse_str(
            r"
version: 2
command: [service]
start_condition:
  command: [/bin/false]
  timeout_seconds: 0
  backoff:
    initial_seconds: 5
    max_seconds: 1
",
        );
        assert!(matches!(invalid, Err(ConfigError::Validation(_))));
        Ok(())
    }

    #[test]
    fn validates_commands_dependencies_and_descriptor_tracking() -> Result<(), Box<dyn Error>> {
        let invalid = parse_str(
            r"
version: 2
command: []
requires: [../escape, duplicate, duplicate]
process_mode: descriptor-tracking
",
        );
        let Err(ConfigError::Validation(errors)) = invalid else {
            return Err(
                io::Error::other("invalid service did not produce validation errors").into(),
            );
        };
        assert!(errors.iter().any(|error| error.contains("executable")));
        assert!(
            errors
                .iter()
                .any(|error| error.contains("safe service name"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("duplicate service"))
        );
        assert!(
            errors
                .iter()
                .any(|error| error.contains("stop and reload hooks"))
        );
        Ok(())
    }

    #[test]
    fn descriptor_tracking_requires_bounded_stop_and_reload_hooks() -> Result<(), Box<dyn Error>> {
        let result =
            parse_str("version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\n");
        assert!(matches!(result, Err(ConfigError::Validation(_))));

        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [/usr/local/bin/service-stop]\n    timeout_seconds: 30\n  reload:\n    command: [/usr/local/bin/service-reload]\n    timeout_seconds: 10\n  lifetime_timeout_seconds: 30\n",
        )?;
        assert_eq!(config.process_mode, ProcessMode::DescriptorTracking);
        let tracking = config
            .descriptor_tracking
            .ok_or_else(|| io::Error::other("descriptor tracking configuration is absent"))?;
        assert_eq!(tracking.stop.timeout_seconds, 30);
        assert_eq!(tracking.reload.timeout_seconds, 10);
        assert_eq!(tracking.lifetime_timeout_seconds, 30);
        Ok(())
    }

    #[test]
    fn descriptor_tracking_rejects_partial_misplaced_and_unknown_fields() {
        for source in [
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: []\n    timeout_seconds: 0\n  reload:\n    command: []\n    timeout_seconds: 0\n  lifetime_timeout_seconds: 0\n",
            "version: 2\ncommand: [/bin/true]\ndescriptor_tracking:\n  stop:\n    command: [/bin/true]\n    timeout_seconds: 1\n  reload:\n    command: [/bin/true]\n    timeout_seconds: 1\n  lifetime_timeout_seconds: 1\n",
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [/bin/true]\n    timeout_seconds: 1\n  lifetime_timeout_seconds: 1\n",
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [/bin/true]\n    timeout_seconds: 1\n  reload:\n    command: [/bin/true]\n    timeout_seconds: 1\n  lifetime_timeout_seconds: 1\n  future: true\n",
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [/bin/true]\n    timeout_seconds: 86401\n  reload:\n    command: [/bin/true]\n    timeout_seconds: 1\n  lifetime_timeout_seconds: 86401\n",
        ] {
            assert!(parse_str(source).is_err());
        }
    }
}
