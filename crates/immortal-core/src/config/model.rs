//! Canonical, validated service configuration model.
//!
//! Every type here is the single in-memory representation produced by
//! strict `version: 2` parsing (see [`super::document`] and
//! [`super::logging`]) and consumed by process setup, path resolution, and
//! serialization back to the canonical schema. [`ServiceConfig::for_command`]
//! and [`LoggingConfig::for_direct`] build the same validated model directly
//! for CLI-driven direct commands, applying the identical policy bounds
//! enforced by [`super::validate`] so every caller observes one validation
//! contract regardless of origin.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize, Serializer, ser::SerializeMap};

use super::{
    ConfigError, DEFAULT_LOG_KEEP, MEBIBYTE,
    logging::{format_log_age, format_log_size},
    validate::{validate, validate_argv, validate_file_log},
};

const DEFAULT_BACKOFF_INITIAL_SECONDS: u64 = 1;
const DEFAULT_BACKOFF_MAX_SECONDS: u64 = 60;
const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;
const DEFAULT_BACKOFF_JITTER_PERCENT: u8 = 20;
const DEFAULT_BACKOFF_RESET_SECONDS: u64 = 60;
const DEFAULT_READINESS_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_CONDITION_BACKOFF_MAX_SECONDS: u64 = 30;

/// Validated runtime service definition.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ServiceConfig {
    /// Whether directory reconciliation should keep this service running.
    pub enabled: bool,
    /// Executable followed by its arguments. No shell interpretation occurs.
    pub command: Vec<String>,
    /// Working directory applied before execution.
    pub working_directory: Option<PathBuf>,
    /// Explicit environment overrides.
    pub environment: BTreeMap<String, String>,
    /// Whether the child inherits the supervisor environment before overrides.
    pub environment_mode: EnvironmentMode,
    /// Account name used for privilege dropping.
    pub user: Option<String>,
    /// Delay before the first start, in seconds.
    pub start_delay_seconds: u64,
    /// Restart and crash-loop policy.
    pub restart: RestartConfig,
    /// Readiness policy for dependency gating.
    pub readiness: ReadinessConfig,
    /// Other services which must become ready before this one starts.
    pub requires: Vec<String>,
    /// Portable command condition evaluated before starting.
    pub start_condition: Option<StartConditionConfig>,
    /// Command invoked after a service generation exits.
    pub post_exit: Option<CommandHook>,
    /// Standard-output and standard-error routing.
    #[serde(flatten)]
    pub logging: LoggingConfig,
    /// Optional output-only PID paths.
    pub pid_files: PidFiles,
    /// Explicit handling for self-daemonizing programs.
    pub process_mode: ProcessMode,
    /// Required lifecycle contract for descriptor-tracked programs.
    pub descriptor_tracking: Option<DescriptorTrackingConfig>,
}

impl ServiceConfig {
    /// Construct the strict runtime model for a direct CLI command.
    ///
    /// Configuration-file-only features remain at their documented defaults;
    /// callers may then apply typed CLI overrides and call [`super::resolve_paths`].
    ///
    /// # Errors
    ///
    /// Returns the same validation error as a `version: 2` document.
    pub fn for_command(command: Vec<String>) -> Result<Self, ConfigError> {
        let config = Self {
            enabled: true,
            command,
            working_directory: None,
            environment: BTreeMap::new(),
            environment_mode: EnvironmentMode::Inherit,
            user: None,
            start_delay_seconds: 0,
            restart: RestartConfig::default(),
            readiness: ReadinessConfig::default(),
            requires: Vec::new(),
            start_condition: None,
            post_exit: None,
            logging: LoggingConfig::default(),
            pid_files: PidFiles::default(),
            process_mode: ProcessMode::Foreground,
            descriptor_tracking: None,
        };
        validate(&config)?;
        Ok(config)
    }
}

/// Policy controlling whether a completed service generation is restarted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicy {
    /// Restart regardless of exit result.
    Always,
    /// Restart only when the exit result is not successful.
    OnFailure,
    /// Never restart automatically.
    Never,
}

/// Base environment supplied to a child before explicit overrides.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EnvironmentMode {
    /// Inherit the supervisor environment, then apply configured overrides.
    #[default]
    Inherit,
    /// Start empty, then apply only configured overrides.
    Clear,
}

/// Crash-loop delay settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct BackoffConfig {
    /// Delay after the first failed generation.
    pub initial_seconds: u64,
    /// Upper bound for the delay.
    pub max_seconds: u64,
    /// Integer exponential multiplier.
    pub multiplier: u32,
    /// Maximum random variation around a delay, as a percentage.
    pub jitter_percent: u8,
    /// Runtime after which the failure streak is reset.
    pub reset_after_seconds: u64,
}

impl Default for BackoffConfig {
    fn default() -> Self {
        Self {
            initial_seconds: DEFAULT_BACKOFF_INITIAL_SECONDS,
            max_seconds: DEFAULT_BACKOFF_MAX_SECONDS,
            multiplier: DEFAULT_BACKOFF_MULTIPLIER,
            jitter_percent: DEFAULT_BACKOFF_JITTER_PERCENT,
            reset_after_seconds: DEFAULT_BACKOFF_RESET_SECONDS,
        }
    }
}

/// Optional restart limits. Every absent limit means unbounded retries.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestartLimits {
    /// Restarts permitted after the initial start.
    pub max_retries: Option<u32>,
    /// Total elapsed supervision time before further restarts stop.
    pub max_elapsed_seconds: Option<u64>,
    /// Starts permitted inside a rolling window.
    pub burst: Option<RestartBurstLimit>,
}

/// Rolling crash-loop limit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestartBurstLimit {
    /// Maximum starts in the window.
    pub starts: u32,
    /// Rolling window length.
    pub window_seconds: u64,
}

/// Restart policy and successful exit semantics.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct RestartConfig {
    /// Restart selection policy.
    pub policy: RestartPolicy,
    /// Exit codes treated as successful.
    pub success_exit_codes: BTreeSet<u8>,
    /// Exit after terminal completion or exhaustion of a configured service restart limit.
    pub exit_when_done: bool,
    /// Explicit safeguards against unbounded crash loops.
    pub limits: RestartLimits,
    /// Delay policy between attempts.
    pub backoff: BackoffConfig,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            policy: RestartPolicy::Always,
            success_exit_codes: BTreeSet::from([0]),
            exit_when_done: false,
            limits: RestartLimits::default(),
            backoff: BackoffConfig::default(),
        }
    }
}

/// Mechanism used to decide when a child is ready.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReadinessMode {
    /// The child is ready as soon as it has been executed successfully.
    #[default]
    Immediate,
    /// The child closes a descriptor inherited through `IMMORTAL_READY_FD`.
    NotifyFd,
}

/// Readiness deadline and mechanism.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ReadinessConfig {
    /// Readiness mechanism.
    pub mode: ReadinessMode,
    /// Maximum time to wait for readiness.
    pub timeout_seconds: u64,
}

impl Default for ReadinessConfig {
    fn default() -> Self {
        Self {
            mode: ReadinessMode::Immediate,
            timeout_seconds: DEFAULT_READINESS_TIMEOUT_SECONDS,
        }
    }
}

/// An argv command with a hard deadline.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandHook {
    /// Executable followed by arguments.
    pub command: Vec<String>,
    /// Maximum execution time.
    pub timeout_seconds: u64,
}

/// Lifecycle commands required when no adopted process ID is trusted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DescriptorTrackingConfig {
    /// Command which asks the self-daemonized application to stop.
    pub stop: CommandHook,
    /// Command which asks the self-daemonized application to reload.
    pub reload: CommandHook,
    /// Maximum wait for lifetime-descriptor EOF after a successful stop hook.
    pub lifetime_timeout_seconds: u64,
}

/// Backoff applied only to failed pre-start condition evaluations.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ConditionBackoffConfig {
    /// Delay after the first failed condition.
    pub initial_seconds: u64,
    /// Upper bound for the delay.
    pub max_seconds: u64,
    /// Integer exponential multiplier.
    pub multiplier: u32,
    /// Maximum random variation around a delay, as a percentage.
    pub jitter_percent: u8,
}

impl Default for ConditionBackoffConfig {
    fn default() -> Self {
        Self {
            initial_seconds: DEFAULT_BACKOFF_INITIAL_SECONDS,
            max_seconds: DEFAULT_CONDITION_BACKOFF_MAX_SECONDS,
            multiplier: DEFAULT_BACKOFF_MULTIPLIER,
            jitter_percent: DEFAULT_BACKOFF_JITTER_PERCENT,
        }
    }
}

/// Portable argv condition evaluated before allocating a service attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StartConditionConfig {
    /// Executable followed by arguments.
    pub command: Vec<String>,
    /// Maximum duration of one evaluation.
    pub timeout_seconds: u64,
    /// Retry delay independent of service restart backoff.
    #[serde(default)]
    pub backoff: ConditionBackoffConfig,
}

/// One validated local-file destination and its rotation policy.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileLogConfig {
    /// Destination file.
    pub file: PathBuf,
    /// Rotation age in seconds.
    pub max_age_seconds: Option<u64>,
    /// Number of rotated files retained.
    pub keep: Option<u32>,
    /// Rotation threshold in bytes.
    pub max_bytes: Option<u64>,
    /// Prefix records with timestamps.
    pub timestamp: bool,
}

impl Serialize for FileLogConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let fields = 1
            + self.max_age_seconds.iter().count()
            + self.keep.iter().count()
            + self.max_bytes.iter().count()
            + usize::from(self.timestamp);
        let mut map = serializer.serialize_map(Some(fields))?;
        map.serialize_entry("file", &self.file)?;
        if let Some(seconds) = self.max_age_seconds {
            map.serialize_entry("age", &format_log_age(seconds))?;
        }
        if let Some(keep) = self.keep {
            map.serialize_entry("keep", &keep)?;
        }
        if let Some(bytes) = self.max_bytes {
            map.serialize_entry("size", &format_log_size(bytes))?;
        }
        if self.timestamp {
            map.serialize_entry("timestamp", &true)?;
        }
        map.end()
    }
}

/// Strict local-file routing selected by the YAML `log` shape.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(untagged)]
pub enum FileLogRoutes {
    /// One file receives combined stdout and stderr.
    Combined(FileLogConfig),
    /// Only explicitly named streams receive local file adapters.
    Selected {
        /// Optional stdout-only destination.
        #[serde(skip_serializing_if = "Option::is_none")]
        stdout: Option<FileLogConfig>,
        /// Optional stderr-only destination.
        #[serde(skip_serializing_if = "Option::is_none")]
        stderr: Option<FileLogConfig>,
    },
}

/// Retry policy for the single external logger process.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggerRestartConfig {
    /// Restarts permitted after the initial logger start; absent means unbounded.
    pub max_retries: Option<u32>,
    /// Delay and stable-runtime reset policy for consecutive logger failures.
    pub backoff: BackoffConfig,
}

/// Output configuration for both child streams.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct LoggingConfig {
    /// Optional local-file routes.
    #[serde(rename = "log", skip_serializing_if = "Option::is_none")]
    pub files: Option<FileLogRoutes>,
    /// Optional external logger argv receiving combined stdout and stderr.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub logger: Option<Vec<String>>,
    /// Optional external file-adapter executable; defaults to sibling `immortallog`.
    #[serde(rename = "log_adapter", skip_serializing_if = "Option::is_none")]
    pub file_adapter: Option<PathBuf>,
    /// Restart and exhaustion policy for the external logger.
    #[serde(
        rename = "logger_restart",
        skip_serializing_if = "logger_restart_is_default"
    )]
    pub restart: LoggerRestartConfig,
}

impl LoggingConfig {
    /// Build direct-command logging with the historical safe file defaults.
    ///
    /// A logfile receives combined stdout and stderr, rotates at 1 MiB, and
    /// retains seven archives. An external logger, when present, receives the
    /// same combined bytes and may coexist with the local file.
    ///
    /// # Errors
    ///
    /// Returns a validation error for an empty or NUL-containing logger argv or
    /// an empty logfile path.
    pub fn for_direct(
        logfile: Option<PathBuf>,
        logger: Option<Vec<String>>,
    ) -> Result<Self, ConfigError> {
        let files = logfile.map(|file| {
            FileLogRoutes::Combined(FileLogConfig {
                file,
                max_age_seconds: None,
                keep: Some(DEFAULT_LOG_KEEP),
                max_bytes: Some(MEBIBYTE),
                timestamp: false,
            })
        });
        let config = Self {
            files,
            logger,
            file_adapter: None,
            restart: LoggerRestartConfig::default(),
        };
        let mut errors = Vec::new();
        if let Some(logger) = &config.logger {
            validate_argv(logger, "logger", &mut errors);
        }
        if let Some(FileLogRoutes::Combined(file)) = &config.files {
            validate_file_log(file, "log", &mut errors);
        }
        if errors.is_empty() {
            Ok(config)
        } else {
            Err(ConfigError::Validation(errors))
        }
    }
}

fn logger_restart_is_default(restart: &LoggerRestartConfig) -> bool {
    restart == &LoggerRestartConfig::default()
}

/// PID paths retained as output-only compatibility metadata.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct PidFiles {
    /// Supervisor PID output path.
    pub supervisor: Option<PathBuf>,
    /// Main child PID output path.
    pub main: Option<PathBuf>,
}

/// Ownership model for the supervised application.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProcessMode {
    /// The service remains a direct child in the managed process group.
    #[default]
    Foreground,
    /// A descriptor remains open across the application's own daemonization.
    DescriptorTracking,
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use super::{FileLogRoutes, LoggingConfig};

    #[test]
    fn direct_logging_preserves_go_rotation_defaults() -> Result<(), Box<dyn Error>> {
        let logging = LoggingConfig::for_direct(
            Some(PathBuf::from("/tmp/app.log")),
            Some(vec!["/bin/cat".to_owned()]),
        )?;
        let Some(FileLogRoutes::Combined(file)) = logging.files else {
            return Err("direct combined file route is missing".into());
        };
        assert_eq!(file.max_bytes, Some(1_048_576));
        assert_eq!(file.keep, Some(7));
        assert_eq!(logging.logger, Some(vec!["/bin/cat".to_owned()]));
        Ok(())
    }
}
