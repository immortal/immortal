//! Every accepted logging wire shape and their reconciliation into one config.
//!
//! The `log`, `logger`, `stderr`, and deprecated `logging` fields accepted by
//! [`super::super::document::ConfigDocument`] each describe local file
//! routing or external logger delegation differently. [`normalize_logging`]
//! is the single place that reconciles them into one canonical
//! [`super::super::model::LoggingConfig`]: deprecated v2 shapes are
//! translated only when their stream behavior is representable without
//! widening or dropping output, and every remaining ambiguity becomes a
//! validation error instead of a silent guess.

use std::path::PathBuf;

use serde::Deserialize;

use super::super::{ConfigError, FileLogConfig, FileLogRoutes, LoggerRestartConfig, LoggingConfig};
use super::quantity::{deserialize_optional_log_age, deserialize_optional_log_size};

/// Untagged `log` shape selecting either one combined route or explicit streams.
#[derive(Deserialize)]
#[serde(untagged)]
pub(in super::super) enum LogInput {
    Combined(FileLogInput),
    Selected(SelectedLogInput),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct SelectedLogInput {
    stdout: Option<FileLogInput>,
    stderr: Option<FileLogInput>,
}

/// Wire shape of one local-file destination before default and unit normalization.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in super::super) struct FileLogInput {
    file: PathBuf,
    #[serde(default, deserialize_with = "deserialize_optional_log_age")]
    age: Option<u64>,
    #[serde(default, alias = "num")]
    keep: Option<u32>,
    #[serde(default, deserialize_with = "deserialize_optional_log_size")]
    size: Option<u64>,
    #[serde(default)]
    timestamp: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct LegacyFileLogConfig {
    file: Option<PathBuf>,
    max_age_seconds: Option<u64>,
    keep: Option<u32>,
    max_bytes: Option<u64>,
    max_total_bytes: Option<u64>,
    timestamp: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct LegacyOutputConfig {
    file: LegacyFileLogConfig,
    logger: Option<Vec<String>>,
}

/// Deprecated Rust v2 `logging` shape accepted during migration.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub(in super::super) struct LegacyLoggingConfig {
    file_adapter: Option<PathBuf>,
    combine_stderr: bool,
    restart: LoggerRestartConfig,
    stdout: LegacyOutputConfig,
    stderr: LegacyOutputConfig,
}

/// Reconcile every accepted logging alias into one canonical [`LoggingConfig`].
///
/// # Errors
///
/// Returns a validation error when the deprecated `logging` shape is mixed
/// with any canonical field, when required destinations or units are
/// missing or malformed, or when a route cannot be represented without
/// widening or dropping output.
pub(in super::super) fn normalize_logging(
    log: Option<LogInput>,
    logger: Option<Vec<String>>,
    file_adapter: Option<PathBuf>,
    logger_restart: Option<LoggerRestartConfig>,
    stderr_alias: Option<FileLogInput>,
    legacy: Option<LegacyLoggingConfig>,
) -> Result<LoggingConfig, ConfigError> {
    if let Some(legacy) = legacy {
        if log.is_some()
            || logger.is_some()
            || file_adapter.is_some()
            || logger_restart.is_some()
            || stderr_alias.is_some()
        {
            return Err(logging_validation(
                "deprecated `logging` cannot be mixed with `log`, `stderr`, `logger`, \
                 `log_adapter`, or `logger_restart`",
            ));
        }
        return normalize_legacy_logging(legacy);
    }

    let files = normalize_file_routes(log, stderr_alias)?;
    if file_adapter.is_some() && files.is_none() {
        return Err(logging_validation("log_adapter requires a log destination"));
    }
    if logger_restart.is_some() && logger.is_none() {
        return Err(logging_validation("logger_restart requires logger"));
    }
    Ok(LoggingConfig {
        files,
        logger,
        file_adapter,
        restart: logger_restart.unwrap_or_default(),
    })
}

fn normalize_file_routes(
    log: Option<LogInput>,
    stderr_alias: Option<FileLogInput>,
) -> Result<Option<FileLogRoutes>, ConfigError> {
    match (log, stderr_alias) {
        (None, None) => Ok(None),
        (None, Some(stderr)) => Ok(Some(FileLogRoutes::Selected {
            stdout: None,
            stderr: Some(stderr.into_config()),
        })),
        (Some(LogInput::Combined(stdout)), None) => {
            Ok(Some(FileLogRoutes::Combined(stdout.into_config())))
        }
        (Some(LogInput::Combined(stdout)), Some(stderr)) => Ok(Some(FileLogRoutes::Selected {
            stdout: Some(stdout.into_config()),
            stderr: Some(stderr.into_config()),
        })),
        (Some(LogInput::Selected(selected)), None) => {
            if selected.stdout.is_none() && selected.stderr.is_none() {
                return Err(logging_validation(
                    "log must contain a file, stdout, or stderr destination",
                ));
            }
            Ok(Some(FileLogRoutes::Selected {
                stdout: selected.stdout.map(FileLogInput::into_config),
                stderr: selected.stderr.map(FileLogInput::into_config),
            }))
        }
        (Some(LogInput::Selected(_)), Some(_)) => Err(logging_validation(
            "top-level stderr duplicates or conflicts with nested log.stderr",
        )),
    }
}

impl FileLogInput {
    fn into_config(self) -> FileLogConfig {
        FileLogConfig {
            file: self.file,
            max_age_seconds: self.age,
            keep: normalized_keep(self.age, self.size, self.keep),
            max_bytes: self.size,
            timestamp: self.timestamp,
        }
    }
}

fn normalized_keep(age: Option<u64>, size: Option<u64>, keep: Option<u32>) -> Option<u32> {
    if keep.is_none() && (age.is_some() || size.is_some()) {
        Some(super::super::DEFAULT_LOG_KEEP)
    } else {
        keep
    }
}

fn normalize_legacy_logging(legacy: LegacyLoggingConfig) -> Result<LoggingConfig, ConfigError> {
    let LegacyLoggingConfig {
        file_adapter,
        combine_stderr,
        restart,
        stdout,
        stderr,
    } = legacy;
    if combine_stderr && legacy_output_is_configured(&stderr) {
        return Err(logging_validation(
            "deprecated logging.combine_stderr conflicts with an explicit stderr route",
        ));
    }

    let LegacyOutputConfig {
        file: stdout_file,
        logger: stdout_logger,
    } = stdout;
    let LegacyOutputConfig {
        file: stderr_file,
        logger: stderr_logger,
    } = stderr;
    let stdout_file = normalize_legacy_file(stdout_file, "logging.stdout.file")?;
    let stderr_file = normalize_legacy_file(stderr_file, "logging.stderr.file")?;

    let files = if combine_stderr {
        stdout_file.map(FileLogRoutes::Combined)
    } else if stdout_file.is_some() || stderr_file.is_some() {
        Some(FileLogRoutes::Selected {
            stdout: stdout_file,
            stderr: stderr_file,
        })
    } else {
        None
    };

    let logger = if combine_stderr {
        stdout_logger
    } else {
        match (stdout_logger, stderr_logger) {
            (None, None) => None,
            (Some(stdout), Some(stderr)) if stdout == stderr => Some(stdout),
            (Some(_), Some(_)) => {
                return Err(logging_validation(
                    "deprecated per-stream logger commands differ; use one top-level logger argv",
                ));
            }
            (Some(_), None) | (None, Some(_)) => {
                return Err(logging_validation(
                    "deprecated single-stream logger routing cannot map to combined top-level \
                     logger",
                ));
            }
        }
    };

    if file_adapter.is_some() && files.is_none() {
        return Err(logging_validation(
            "deprecated logging.file_adapter requires a file destination",
        ));
    }
    if logger.is_none() && restart != LoggerRestartConfig::default() {
        return Err(logging_validation(
            "deprecated logging.restart requires an external logger",
        ));
    }
    Ok(LoggingConfig {
        files,
        logger,
        file_adapter,
        restart,
    })
}

fn normalize_legacy_file(
    legacy: LegacyFileLogConfig,
    path: &str,
) -> Result<Option<FileLogConfig>, ConfigError> {
    if legacy.max_total_bytes.is_some() {
        return Err(logging_validation(&format!(
            "{path}.max_total_bytes is unsupported; use size and keep"
        )));
    }
    let has_policy = legacy.max_age_seconds.is_some()
        || legacy.keep.is_some()
        || legacy.max_bytes.is_some()
        || legacy.timestamp;
    match legacy.file {
        Some(file) => Ok(Some(FileLogConfig {
            file,
            max_age_seconds: legacy.max_age_seconds,
            keep: normalized_keep(legacy.max_age_seconds, legacy.max_bytes, legacy.keep),
            max_bytes: legacy.max_bytes,
            timestamp: legacy.timestamp,
        })),
        None if has_policy => Err(logging_validation(&format!(
            "{path} rotation options require a destination file"
        ))),
        None => Ok(None),
    }
}

fn legacy_output_is_configured(output: &LegacyOutputConfig) -> bool {
    output.file.file.is_some()
        || output.file.max_age_seconds.is_some()
        || output.file.keep.is_some()
        || output.file.max_bytes.is_some()
        || output.file.max_total_bytes.is_some()
        || output.file.timestamp
        || output.logger.is_some()
}

fn logging_validation(message: &str) -> ConfigError {
    ConfigError::Validation(vec![message.to_owned()])
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use crate::config::{BackoffConfig, ConfigError, FileLogRoutes, emit_config, parse_str};

    #[test]
    fn logging_schema_normalizes_combined_and_selected_routes() -> Result<(), Box<dyn Error>> {
        let combined = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 86400\n  size: 1\nlogger: [/usr/bin/logger, -t, app]\n",
        )?;
        let Some(FileLogRoutes::Combined(file)) = &combined.logging.files else {
            return Err("combined log route is missing".into());
        };
        assert_eq!(file.file, PathBuf::from("/tmp/app.log"));
        assert_eq!(file.max_age_seconds, Some(86_400));
        assert_eq!(file.max_bytes, Some(1_048_576));
        assert_eq!(file.keep, Some(7));
        assert_eq!(
            combined.logging.logger,
            Some(vec![
                "/usr/bin/logger".to_owned(),
                "-t".to_owned(),
                "app".to_owned()
            ])
        );
        let emitted = emit_config(&combined)?;
        assert!(emitted.contains("age: 1d"));
        assert!(emitted.contains("size: 1MiB"));
        assert!(emitted.contains("keep: 7"));
        assert!(!emitted.lines().any(|line| line == "logging:"));

        let selected = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  stderr:\n    file: /tmp/app.err\n",
        )?;
        let Some(FileLogRoutes::Selected { stdout, stderr }) = &selected.logging.files else {
            return Err("selected log routes are missing".into());
        };
        assert_eq!(stdout, &None);
        assert_eq!(
            stderr.as_ref().map(|file| &file.file),
            Some(&PathBuf::from("/tmp/app.err"))
        );
        Ok(())
    }

    #[test]
    fn logging_schema_rejects_ambiguous_ineffective_and_unbounded_values() {
        for source in [
            "version: 2\ncommand: [/bin/true]\nlog: {}\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  stdout:\n    file: /tmp/out.log\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  keep: 7\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 0\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 01s\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 1month\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  size: 0\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  size: 1.5MiB\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  size: 1MB\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  size: 18446744073709551615GiB\n",
            "version: 2\ncommand: [/bin/true]\nlog_adapter: /bin/cat\n",
            "version: 2\ncommand: [/bin/true]\nlogger_restart:\n  max_retries: 1\n",
            "version: 2\ncommand: [/bin/true]\nlogger: []\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  stdout:\n    file: /tmp/out.log\n    logger: [/bin/cat]\n",
        ] {
            assert!(
                parse_str(source).is_err(),
                "unexpected valid source: {source}"
            );
        }
    }

    #[test]
    fn logging_compatibility_aliases_emit_only_canonical_fields() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 86400\n  num: 7\n  size: 1\nstderr:\n  file: /tmp/app.err\n",
        )?;
        let Some(FileLogRoutes::Selected {
            stdout: Some(stdout),
            stderr: Some(stderr),
        }) = &config.logging.files
        else {
            return Err("Go-compatible split routes are missing".into());
        };
        assert_eq!(stdout.file, PathBuf::from("/tmp/app.log"));
        assert_eq!(stderr.file, PathBuf::from("/tmp/app.err"));
        let emitted = emit_config(&config)?;
        assert!(emitted.lines().any(|line| line == "  stderr:"));
        assert!(!emitted.lines().any(|line| line == "stderr:"));
        assert!(!emitted.contains("num:"));

        let legacy = parse_str(
            "version: 2\ncommand: [/bin/true]\nlogging:\n  combine_stderr: true\n  stdout:\n    file:\n      file: /tmp/app.log\n      max_age_seconds: 60\n      max_bytes: 1048576\n      keep: 7\n    logger: [/bin/cat]\n",
        )?;
        assert!(matches!(
            legacy.logging.files,
            Some(FileLogRoutes::Combined(_))
        ));
        assert_eq!(legacy.logging.logger, Some(vec!["/bin/cat".to_owned()]));
        let emitted = emit_config(&legacy)?;
        assert!(!emitted.lines().any(|line| line == "logging:"));
        assert!(emitted.lines().any(|line| line == "log:"));
        assert!(emitted.lines().any(|line| line == "logger:"));
        Ok(())
    }

    #[test]
    fn logging_compatibility_rejects_conflicts_and_unrepresentable_routes() {
        for source in [
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  keep: 7\n  num: 7\n  size: 1MiB\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  stderr:\n    file: /tmp/app.err\nstderr:\n  file: /tmp/legacy.err\n",
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\nlogging: {}\n",
            "version: 2\ncommand: [/bin/true]\nlogging:\n  stdout:\n    file:\n      file: /tmp/app.log\n      max_total_bytes: 1048576\n",
            "version: 2\ncommand: [/bin/true]\nlogging:\n  stdout:\n    logger: [/bin/cat, stdout]\n  stderr:\n    logger: [/bin/cat, stderr]\n",
            "version: 2\ncommand: [/bin/true]\nlogging:\n  stdout:\n    logger: [/bin/cat]\n",
        ] {
            assert!(
                parse_str(source).is_err(),
                "unexpected valid source: {source}"
            );
        }
    }

    #[test]
    fn logger_restart_policy_parses_defaults_bounds_and_unknown_fields()
    -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            r"
version: 2
command: [service]
logger: [/bin/cat]
logger_restart:
  max_retries: 4
  backoff:
    initial_seconds: 2
    max_seconds: 8
    multiplier: 3
    jitter_percent: 5
    reset_after_seconds: 20
",
        )?;
        assert_eq!(config.logging.restart.max_retries, Some(4));
        assert_eq!(config.logging.restart.backoff.initial_seconds, 2);
        assert_eq!(config.logging.restart.backoff.max_seconds, 8);
        assert_eq!(config.logging.restart.backoff.multiplier, 3);
        assert_eq!(config.logging.restart.backoff.jitter_percent, 5);
        assert_eq!(config.logging.restart.backoff.reset_after_seconds, 20);

        let defaults = parse_str("version: 2\ncommand: [service]\n")?;
        assert_eq!(defaults.logging.restart.max_retries, None);
        assert_eq!(defaults.logging.restart.backoff, BackoffConfig::default());
        assert!(matches!(
            parse_str(
                "version: 2\ncommand: [service]\nlogger: [/bin/cat]\nlogger_restart:\n  backoff:\n    initial_seconds: 0\n"
            ),
            Err(ConfigError::Validation(_))
        ));
        assert!(matches!(
            parse_str(
                "version: 2\ncommand: [service]\nlogger: [/bin/cat]\nlogger_restart:\n  future: true\n"
            ),
            Err(ConfigError::Parse(_))
        ));
        Ok(())
    }
}
