//! Service configuration types, parsing, validation, and legacy resolution.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use serde::de::IgnoredAny;
use serde::{Deserialize, Serialize};

/// Maximum accepted size of one service definition.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

const DEFAULT_BACKOFF_INITIAL_SECONDS: u64 = 1;
const DEFAULT_BACKOFF_MAX_SECONDS: u64 = 60;
const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;
const DEFAULT_BACKOFF_JITTER_PERCENT: u8 = 20;
const DEFAULT_BACKOFF_RESET_SECONDS: u64 = 60;
const DEFAULT_READINESS_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_CONDITION_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_CONDITION_BACKOFF_MAX_SECONDS: u64 = 30;

/// Source schema used to produce a normalized configuration.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SchemaVersion {
    /// The unversioned YAML schema released by the Go implementation.
    V1,
    /// The strict, argv-based Rust schema.
    V2,
}

/// A non-fatal migration or compatibility diagnostic.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigWarning {
    /// Configuration path related to this warning, when one is available.
    pub path: Option<String>,
    /// Human-readable action for an operator.
    pub message: String,
}

/// A parsed and normalized service definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParsedConfig {
    /// Schema detected in the source document.
    pub source: SchemaVersion,
    /// Normalized configuration consumed by the supervisor.
    pub service: ServiceConfig,
    /// Migration and compatibility warnings.
    pub warnings: Vec<ConfigWarning>,
}

/// Runtime service definition independent of its source schema.
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
    pub logging: LoggingConfig,
    /// Compatibility PID output paths.
    pub pid_files: PidFiles,
    /// Explicit handling for self-daemonizing legacy programs.
    pub process_mode: ProcessMode,
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
    /// Exit the supervisor after a result for which no restart is required.
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

/// File logger compatibility options.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct FileLogConfig {
    /// Destination file.
    pub file: Option<PathBuf>,
    /// Rotation age in seconds.
    pub max_age_seconds: Option<u64>,
    /// Number of rotated files retained.
    pub keep: Option<u32>,
    /// Rotation threshold in bytes.
    pub max_bytes: Option<u64>,
    /// Maximum combined bytes retained across rotated archives.
    pub max_total_bytes: Option<u64>,
    /// Prefix records with timestamps.
    pub timestamp: bool,
}

/// Routing for one output stream.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct OutputConfig {
    /// Optional file compatibility adapter.
    pub file: FileLogConfig,
    /// Optional external logger argv.
    pub logger: Option<Vec<String>>,
}

/// Output configuration for both child streams.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Route stderr through the stdout pipeline instead of creating a second pipe.
    pub combine_stderr: bool,
    /// Standard-output route.
    pub stdout: OutputConfig,
    /// Standard-error route.
    pub stderr: OutputConfig,
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
    /// A legacy descriptor remains open across the application's own daemonization.
    DescriptorTracking,
}

/// Configuration parsing or validation failure.
#[derive(Debug)]
pub enum ConfigError {
    /// Input exceeded [`MAX_CONFIG_BYTES`].
    TooLarge { actual: u64 },
    /// The file could not be read.
    Io(io::Error),
    /// YAML was malformed or did not match the selected schema.
    Parse(String),
    /// A version marker was present but unsupported.
    UnsupportedVersion(u64),
    /// The document parsed but violated runtime invariants.
    Validation(Vec<String>),
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { actual } => write!(
                formatter,
                "configuration is {actual} bytes; limit is {MAX_CONFIG_BYTES} bytes"
            ),
            Self::Io(error) => write!(formatter, "unable to read configuration: {error}"),
            Self::Parse(error) => write!(formatter, "invalid YAML configuration: {error}"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported configuration version {version}")
            }
            Self::Validation(errors) => {
                write!(
                    formatter,
                    "invalid service configuration: {}",
                    errors.join("; ")
                )
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::TooLarge { .. }
            | Self::Parse(_)
            | Self::UnsupportedVersion(_)
            | Self::Validation(_) => None,
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// Read, parse, normalize, and validate a service definition.
///
/// # Errors
///
/// Returns an error when the file cannot be read, exceeds the size limit, is
/// malformed, selects an unsupported schema, or violates service invariants.
pub fn parse_file(path: &Path) -> Result<ParsedConfig, ConfigError> {
    let file = File::open(path)?;
    let declared_size = file.metadata()?.len();
    if declared_size > MAX_CONFIG_BYTES as u64 {
        return Err(ConfigError::TooLarge {
            actual: declared_size,
        });
    }

    let mut bytes = Vec::with_capacity(usize::try_from(declared_size).unwrap_or(0));
    file.take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let mut parsed = parse_bytes(&bytes)?;
    let absolute_source = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let base = absolute_source.parent().ok_or_else(|| {
        ConfigError::Validation(vec![
            "configuration path has no parent directory".to_owned(),
        ])
    })?;
    resolve_paths(&mut parsed.service, base)?;
    validate_resolved(&parsed.service)?;
    Ok(parsed)
}

/// Parse, normalize, and validate an in-memory service definition.
///
/// # Errors
///
/// Returns an error for oversized, non-UTF-8, malformed, unsupported, or
/// semantically invalid input.
pub fn parse_bytes(bytes: &[u8]) -> Result<ParsedConfig, ConfigError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            actual: bytes.len() as u64,
        });
    }
    let source = std::str::from_utf8(bytes)
        .map_err(|error| ConfigError::Parse(format!("input is not UTF-8: {error}")))?;
    parse_str(source)
}

/// Parse, normalize, and validate a UTF-8 service definition.
///
/// # Errors
///
/// Returns an error for malformed, unsupported, or semantically invalid input.
pub fn parse_str(source: &str) -> Result<ParsedConfig, ConfigError> {
    if source.len() > MAX_CONFIG_BYTES {
        return Err(ConfigError::TooLarge {
            actual: source.len() as u64,
        });
    }

    let documents: Vec<IgnoredAny> =
        serde_saphyr::from_multiple_with_options(source, yaml_options())
            .map_err(|error| ConfigError::Parse(error.to_string()))?;
    if documents.len() != 1 {
        return Err(ConfigError::Parse(
            "configuration must contain exactly one YAML document".to_owned(),
        ));
    }

    let header: VersionHeader = serde_saphyr::from_str_with_options(source, yaml_options())
        .map_err(|error| ConfigError::Parse(error.to_string()))?;
    let mut parsed = match header.version {
        None => parse_v1(source)?,
        Some(2) => parse_v2(source)?,
        Some(version) => return Err(ConfigError::UnsupportedVersion(version)),
    };
    validate(&parsed.service)?;
    parsed.warnings.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.message.cmp(&right.message))
    });
    Ok(parsed)
}

/// Serialize a normalized configuration as strict schema v2 YAML.
///
/// # Errors
///
/// Returns an error if the normalized model cannot be represented by the YAML
/// serializer.
pub fn emit_v2(config: &ServiceConfig) -> Result<String, ConfigError> {
    #[derive(Serialize)]
    struct V2Output<'a> {
        version: u8,
        #[serde(flatten)]
        service: &'a ServiceConfig,
    }

    serde_saphyr::to_string(&V2Output {
        version: 2,
        service: config,
    })
    .map_err(|error| ConfigError::Parse(format!("unable to emit schema v2: {error}")))
}

#[derive(Deserialize)]
struct VersionHeader {
    version: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct V2Document {
    version: u8,
    #[serde(default = "default_true")]
    enabled: bool,
    command: Vec<String>,
    working_directory: Option<PathBuf>,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default)]
    environment_mode: EnvironmentMode,
    user: Option<String>,
    #[serde(default)]
    start_delay_seconds: u64,
    #[serde(default)]
    restart: RestartConfig,
    #[serde(default)]
    readiness: ReadinessConfig,
    #[serde(default)]
    requires: Vec<String>,
    start_condition: Option<StartConditionConfig>,
    post_exit: Option<CommandHook>,
    #[serde(default)]
    logging: LoggingConfig,
    #[serde(default)]
    pid_files: PidFiles,
    #[serde(default)]
    process_mode: ProcessMode,
}

fn default_true() -> bool {
    true
}

fn parse_v2(source: &str) -> Result<ParsedConfig, ConfigError> {
    let document: V2Document = serde_saphyr::from_str_with_options(source, yaml_options())
        .map_err(|error| ConfigError::Parse(error.to_string()))?;
    debug_assert_eq!(document.version, 2);
    Ok(ParsedConfig {
        source: SchemaVersion::V2,
        service: ServiceConfig {
            enabled: document.enabled,
            command: document.command,
            working_directory: document.working_directory,
            environment: document.environment,
            environment_mode: document.environment_mode,
            user: document.user,
            start_delay_seconds: document.start_delay_seconds,
            restart: document.restart,
            readiness: document.readiness,
            requires: document.requires,
            start_condition: document.start_condition,
            post_exit: document.post_exit,
            logging: document.logging,
            pid_files: document.pid_files,
            process_mode: document.process_mode,
        },
        warnings: Vec::new(),
    })
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LegacyScalar {
    String(String),
    Signed(i64),
    Unsigned(u64),
    Bool(bool),
}

impl LegacyScalar {
    fn into_string(self) -> String {
        match self {
            Self::String(value) => value,
            Self::Signed(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::Bool(value) => value.to_string(),
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct LegacyDocument {
    cmd: String,
    cwd: Option<PathBuf>,
    env: BTreeMap<String, LegacyScalar>,
    log: LegacyLog,
    stderr: LegacyLog,
    logger: Option<String>,
    require: Vec<String>,
    require_cmd: Option<String>,
    post_exit: Option<String>,
    user: Option<String>,
    wait: u64,
    #[serde(default = "legacy_retries_default")]
    retries: i64,
    pid: LegacyPid,
}

fn legacy_retries_default() -> i64 {
    -1
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct LegacyLog {
    file: Option<PathBuf>,
    age: i64,
    num: i64,
    size: i64,
    timestamp: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct LegacyPid {
    follow: Option<PathBuf>,
    parent: Option<PathBuf>,
    child: Option<PathBuf>,
}

fn parse_v1(source: &str) -> Result<ParsedConfig, ConfigError> {
    let mut ignored = Vec::new();
    let document = deserialize_v1(source, &mut ignored)?;

    let mut warnings = vec![ConfigWarning {
        path: None,
        message: "unversioned schema v1 is deprecated; migrate to version: 2".to_owned(),
    }];
    warnings.extend(ignored.into_iter().map(|path| ConfigWarning {
        message: format!("unknown schema v1 field `{path}` was ignored"),
        path: Some(path),
    }));

    let logger = legacy_logger(document.logger.as_deref(), &mut warnings);
    let start_condition = document.require_cmd.as_deref().map(legacy_start_condition);
    if start_condition.is_some() {
        warnings.push(ConfigWarning {
            path: Some("require_cmd".to_owned()),
            message: "legacy shell condition retained through an explicit shell argv; migrate to a direct argv command"
                .to_owned(),
        });
    }
    let post_exit = document.post_exit.as_deref().map(legacy_shell_hook);
    if post_exit.is_some() {
        warnings.push(ConfigWarning {
            path: Some("post_exit".to_owned()),
            message: "legacy shell hook retained through an explicit shell argv; `$IMMORTAL_EXIT_STATUS` replaces the appended positional argument in v2"
                .to_owned(),
        });
    }

    let process_mode = if document.pid.follow.is_some() {
        warnings.push(ConfigWarning {
            path: Some("pid.follow".to_owned()),
            message:
                "PID adoption is unsafe; descriptor-tracking mode requires explicit lifecycle hooks"
                    .to_owned(),
        });
        ProcessMode::DescriptorTracking
    } else {
        ProcessMode::Foreground
    };
    if document.pid.follow.is_some() {
        warnings.push(ConfigWarning {
            path: Some("pid.follow".to_owned()),
            message: "the followed PID path is not used as process identity".to_owned(),
        });
    }

    let limits = if document.retries < 0 {
        RestartLimits::default()
    } else {
        RestartLimits {
            max_retries: Some(u32::try_from(document.retries).map_err(|_| {
                ConfigError::Validation(vec![
                    "legacy retries exceeds the supported 32-bit limit".to_owned(),
                ])
            })?),
            ..RestartLimits::default()
        }
    };
    let stdout = OutputConfig {
        file: convert_legacy_log(document.log, &mut warnings, "log"),
        logger,
    };
    let combine_stderr = document.stderr.file.is_none();
    let stderr = OutputConfig {
        file: convert_legacy_log(document.stderr, &mut warnings, "stderr"),
        logger: None,
    };

    Ok(ParsedConfig {
        source: SchemaVersion::V1,
        service: ServiceConfig {
            enabled: true,
            command: split_legacy_command(&document.cmd),
            working_directory: document.cwd,
            environment: document
                .env
                .into_iter()
                .map(|(key, value)| (key, value.into_string()))
                .collect(),
            environment_mode: EnvironmentMode::Inherit,
            user: document.user,
            start_delay_seconds: document.wait,
            restart: RestartConfig {
                limits,
                ..RestartConfig::default()
            },
            readiness: ReadinessConfig::default(),
            requires: document.require,
            start_condition,
            post_exit,
            logging: LoggingConfig {
                combine_stderr,
                stdout,
                stderr,
            },
            pid_files: PidFiles {
                supervisor: document.pid.parent,
                main: document.pid.child,
            },
            process_mode,
        },
        warnings,
    })
}

fn deserialize_v1(source: &str, ignored: &mut Vec<String>) -> Result<LegacyDocument, ConfigError> {
    serde_saphyr::with_deserializer_from_str_with_options(source, yaml_options(), |deserializer| {
        serde_ignored::deserialize(deserializer, |path| ignored.push(path.to_string()))
    })
    .map_err(|error| ConfigError::Parse(error.to_string()))
}

fn yaml_options() -> serde_saphyr::Options {
    serde_saphyr::options! {
        budget: serde_saphyr::budget! {
            max_events: 100_000,
            max_aliases: 128,
            max_anchors: 128,
            max_depth: 32,
            max_documents: 2,
            max_nodes: 50_000,
            max_total_scalar_bytes: MAX_CONFIG_BYTES,
            max_total_comment_bytes: MAX_CONFIG_BYTES,
            max_merge_keys: 128,
        },
        alias_limits: serde_saphyr::alias_limits! {
            max_total_replayed_events: 100_000,
            max_replay_stack_depth: 32,
            max_alias_expansions_per_anchor: 128,
        },
    }
}

fn split_legacy_command(command: &str) -> Vec<String> {
    command.split_whitespace().map(ToOwned::to_owned).collect()
}

fn legacy_logger(command: Option<&str>, warnings: &mut Vec<ConfigWarning>) -> Option<Vec<String>> {
    command.map(|value| {
        warnings.push(ConfigWarning {
            path: Some("logger".to_owned()),
            message: "legacy logger text is split on whitespace; use an argv array in v2"
                .to_owned(),
        });
        split_legacy_command(value)
    })
}

fn legacy_shell_hook(command: &str) -> CommandHook {
    CommandHook {
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), command.to_owned()],
        timeout_seconds: 30,
    }
}

fn legacy_start_condition(command: &str) -> StartConditionConfig {
    StartConditionConfig {
        command: vec!["/bin/sh".to_owned(), "-c".to_owned(), command.to_owned()],
        timeout_seconds: DEFAULT_CONDITION_TIMEOUT_SECONDS,
        backoff: ConditionBackoffConfig::default(),
    }
}

fn convert_legacy_log(
    legacy: LegacyLog,
    warnings: &mut Vec<ConfigWarning>,
    path: &str,
) -> FileLogConfig {
    let max_age_seconds = positive_i64(legacy.age, warnings, &format!("{path}.age"));
    let keep = positive_i64(legacy.num, warnings, &format!("{path}.num"))
        .and_then(|value| u32::try_from(value).ok());
    let max_bytes = positive_i64(legacy.size, warnings, &format!("{path}.size"))
        .and_then(|value| value.checked_mul(1024 * 1024));
    FileLogConfig {
        file: legacy.file,
        max_age_seconds,
        keep,
        max_bytes,
        max_total_bytes: None,
        timestamp: legacy.timestamp,
    }
}

fn positive_i64(value: i64, warnings: &mut Vec<ConfigWarning>, path: &str) -> Option<u64> {
    match value {
        0 => None,
        ..=-1 => {
            warnings.push(ConfigWarning {
                path: Some(path.to_owned()),
                message: "negative legacy value is invalid and will be rejected by schema v2"
                    .to_owned(),
            });
            None
        }
        1.. => u64::try_from(value).ok(),
    }
}

fn validate(config: &ServiceConfig) -> Result<(), ConfigError> {
    let mut errors = Vec::new();
    validate_argv(&config.command, "command", &mut errors);
    validate_optional_text(config.user.as_deref(), "user", &mut errors);
    for (key, value) in &config.environment {
        if key.is_empty() || key.contains('=') || key.contains('\0') {
            errors.push(format!("environment key `{key}` is invalid"));
        }
        if value.contains('\0') {
            errors.push(format!("environment value for `{key}` contains NUL"));
        }
    }
    validate_service_names(&config.requires, &mut errors);
    if config.restart.success_exit_codes.is_empty() {
        errors.push("restart.success_exit_codes must not be empty".to_owned());
    }
    let backoff = &config.restart.backoff;
    if backoff.initial_seconds == 0 {
        errors.push("restart.backoff.initial_seconds must be greater than zero".to_owned());
    }
    if backoff.max_seconds < backoff.initial_seconds {
        errors.push("restart.backoff.max_seconds must be at least initial_seconds".to_owned());
    }
    if backoff.multiplier < 1 {
        errors.push("restart.backoff.multiplier must be at least one".to_owned());
    }
    if backoff.jitter_percent > 100 {
        errors.push("restart.backoff.jitter_percent must not exceed 100".to_owned());
    }
    if let Some(burst) = &config.restart.limits.burst
        && (burst.starts == 0 || burst.window_seconds == 0)
    {
        errors.push("restart.limits.burst values must be greater than zero".to_owned());
    }
    if config.readiness.timeout_seconds == 0 {
        errors.push("readiness.timeout_seconds must be greater than zero".to_owned());
    }
    if let Some(hook) = &config.start_condition {
        validate_start_condition(hook, &mut errors);
    }
    if let Some(hook) = &config.post_exit {
        validate_hook(hook, "post_exit", &mut errors);
    }
    validate_output(&config.logging.stdout, "logging.stdout", &mut errors);
    validate_output(&config.logging.stderr, "logging.stderr", &mut errors);
    if config.logging.combine_stderr && output_is_configured(&config.logging.stderr) {
        errors.push("logging.combine_stderr conflicts with an explicit stderr route".to_owned());
    }
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

    if config.process_mode == ProcessMode::DescriptorTracking && config.post_exit.is_none() {
        errors.push(
            "descriptor-tracking process mode requires an explicit post_exit lifecycle hook"
                .to_owned(),
        );
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors))
    }
}

/// Resolve every path whose meaning would otherwise change after daemonization.
///
/// Service executable paths containing `/` are relative to the resolved working
/// directory. Hook/logger executables and file/PID paths are relative to the
/// configuration directory. Bare executable names remain available for `PATH`
/// lookup by the process executor.
///
/// # Errors
///
/// Returns an error if an absolute executable path cannot be represented as
/// UTF-8 by the argv-based configuration model.
pub fn resolve_paths(config: &mut ServiceConfig, base: &Path) -> Result<(), ConfigError> {
    let base = normalize_absolute(base, Path::new("."));
    if let Some(directory) = &mut config.working_directory {
        *directory = normalize_absolute(&base, directory);
    }
    let command_base = config.working_directory.as_deref().unwrap_or(&base);
    resolve_argv_executable(&mut config.command, command_base)?;
    if let Some(hook) = &mut config.start_condition {
        resolve_argv_executable(&mut hook.command, &base)?;
    }
    if let Some(hook) = &mut config.post_exit {
        resolve_argv_executable(&mut hook.command, &base)?;
    }
    resolve_output_paths(&mut config.logging.stdout, &base)?;
    resolve_output_paths(&mut config.logging.stderr, &base)?;
    resolve_optional_path(&mut config.pid_files.supervisor, &base);
    resolve_optional_path(&mut config.pid_files.main, &base);
    Ok(())
}

fn resolve_output_paths(output: &mut OutputConfig, base: &Path) -> Result<(), ConfigError> {
    resolve_optional_path(&mut output.file.file, base);
    if let Some(logger) = &mut output.logger {
        resolve_argv_executable(logger, base)?;
    }
    Ok(())
}

fn resolve_optional_path(path: &mut Option<PathBuf>, base: &Path) {
    if let Some(value) = path {
        *value = normalize_absolute(base, value);
    }
}

fn resolve_argv_executable(argv: &mut [String], base: &Path) -> Result<(), ConfigError> {
    let Some(executable) = argv.first_mut() else {
        return Ok(());
    };
    if !executable.as_bytes().contains(&b'/') {
        return Ok(());
    }
    let resolved = normalize_absolute(base, Path::new(executable));
    *executable = resolved.into_os_string().into_string().map_err(|_| {
        ConfigError::Validation(vec![
            "resolved executable path is not valid UTF-8".to_owned(),
        ])
    })?;
    Ok(())
}

fn normalize_absolute(base: &Path, path: &Path) -> PathBuf {
    let candidate = if path.is_absolute() {
        path.to_owned()
    } else {
        base.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in candidate.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    normalized
}

fn validate_resolved(config: &ServiceConfig) -> Result<(), ConfigError> {
    let mut errors = Vec::new();
    if let Some(directory) = &config.working_directory {
        match fs::metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => errors.push(format!(
                "working_directory `{}` is not a directory",
                directory.display()
            )),
            Err(error) => errors.push(format!(
                "working_directory `{}` is unavailable: {error}",
                directory.display()
            )),
        }
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ConfigError::Validation(errors))
    }
}

fn validate_hook(hook: &CommandHook, path: &str, errors: &mut Vec<String>) {
    validate_argv(&hook.command, &format!("{path}.command"), errors);
    if hook.timeout_seconds == 0 {
        errors.push(format!("{path}.timeout_seconds must be greater than zero"));
    }
}

fn validate_start_condition(condition: &StartConditionConfig, errors: &mut Vec<String>) {
    validate_argv(&condition.command, "start_condition.command", errors);
    if condition.timeout_seconds == 0 {
        errors.push("start_condition.timeout_seconds must be greater than zero".to_owned());
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
    if backoff.multiplier == 0 {
        errors.push("start_condition.backoff.multiplier must be at least one".to_owned());
    }
    if backoff.jitter_percent > 100 {
        errors.push("start_condition.backoff.jitter_percent must not exceed 100".to_owned());
    }
}

fn validate_output(output: &OutputConfig, path: &str, errors: &mut Vec<String>) {
    if let Some(logger) = &output.logger {
        validate_argv(logger, &format!("{path}.logger"), errors);
    }
    validate_path(
        output.file.file.as_deref(),
        &format!("{path}.file.file"),
        errors,
    );
    if output.file.max_bytes == Some(0) {
        errors.push(format!("{path}.file.max_bytes must be greater than zero"));
    }
    if output.file.max_total_bytes == Some(0) {
        errors.push(format!(
            "{path}.file.max_total_bytes must be greater than zero"
        ));
    }
    if output.file.keep == Some(0) {
        errors.push(format!("{path}.file.keep must be greater than zero"));
    }
    if output.file.file.is_none()
        && (output.file.max_age_seconds.is_some()
            || output.file.keep.is_some()
            || output.file.max_bytes.is_some()
            || output.file.max_total_bytes.is_some()
            || output.file.timestamp)
    {
        errors.push(format!(
            "{path}.file rotation options require a destination file"
        ));
    }
}

fn output_is_configured(output: &OutputConfig) -> bool {
    output.file.file.is_some() || output.logger.is_some()
}

fn validate_argv(argv: &[String], path: &str, errors: &mut Vec<String>) {
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
        let safe = !name.is_empty()
            && name != "."
            && name != ".."
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
        if !safe {
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
    use std::{error::Error, fs, io};

    use super::{
        ConfigError, EnvironmentMode, MAX_CONFIG_BYTES, ProcessMode, RestartPolicy, SchemaVersion,
        emit_v2, parse_bytes, parse_file, parse_str,
    };

    #[test]
    fn parses_and_normalizes_released_v1_example() -> Result<(), Box<dyn Error>> {
        let parsed = parse_str(include_str!("../tests/fixtures/v1/run.yml"))?;

        assert_eq!(parsed.source, SchemaVersion::V1);
        assert_eq!(
            parsed.service.command,
            ["bundle", "exec", "unicorn", "-c", "unicorn.rb"]
        );
        assert_eq!(
            parsed.service.environment.get("DEBUG").map(String::as_str),
            Some("1")
        );
        assert_eq!(parsed.service.restart.policy, RestartPolicy::Always);
        assert_eq!(parsed.service.restart.limits.max_retries, None);
        assert_eq!(
            parsed.service.logging.stdout.file.max_bytes,
            Some(1_048_576)
        );
        assert_eq!(parsed.service.logging.stdout.file.keep, Some(7));
        assert!(parsed.service.logging.stdout.logger.is_some());
        assert!(parsed.service.logging.stderr.logger.is_none());
        assert!(parsed.service.logging.combine_stderr);
        assert!(!parsed.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn v1_retries_zero_means_no_restart_after_initial_start() -> Result<(), Box<dyn Error>> {
        let parsed = parse_str("cmd: /bin/true\nretries: 0\n")?;
        assert_eq!(parsed.service.restart.limits.max_retries, Some(0));
        Ok(())
    }

    #[test]
    fn rejects_legacy_retry_values_that_cannot_be_enforced() {
        let result = parse_str("cmd: /bin/true\nretries: 4294967296\n");
        assert!(matches!(result, Err(ConfigError::Validation(_))));
    }

    #[test]
    fn released_dependency_and_retry_fixtures_parse() -> Result<(), Box<dyn Error>> {
        let required = parse_str(include_str!("../tests/fixtures/v1/require.yml"))?;
        assert_eq!(required.service.requires, ["foo", "bar"]);
        let retries = parse_str(include_str!("../tests/fixtures/v1/retries.yml"))?;
        assert_eq!(retries.service.restart.limits.max_retries, Some(3));
        Ok(())
    }

    #[test]
    fn released_go_examples_have_explicit_migration_contracts() -> Result<(), Box<dyn Error>> {
        let supported = [
            ("bar", include_str!("../tests/fixtures/go-master/bar.yml")),
            ("foo", include_str!("../tests/fixtures/go-master/foo.yml")),
            (
                "require",
                include_str!("../tests/fixtures/go-master/require.yml"),
            ),
            (
                "require_cmd",
                include_str!("../tests/fixtures/go-master/require_cmd.yml"),
            ),
            (
                "retries",
                include_str!("../tests/fixtures/go-master/retries.yml"),
            ),
            ("test", include_str!("../tests/fixtures/go-master/test.yml")),
            (
                "test_only_stderr",
                include_str!("../tests/fixtures/go-master/test_only_stderr.yml"),
            ),
            (
                "test_stderr",
                include_str!("../tests/fixtures/go-master/test_stderr.yml"),
            ),
        ];
        for (name, source) in supported {
            let parsed = parse_str(source).map_err(|error| {
                io::Error::other(format!("released example {name} failed: {error}"))
            })?;
            let migrated = emit_v2(&parsed.service)?;
            let reparsed = parse_str(&migrated)?;
            assert_eq!(reparsed.service, parsed.service, "fixture {name}");
        }

        for (name, source) in [
            (
                "avoid-logrotate",
                include_str!("../tests/fixtures/go-master/avoid-logrotate.yml"),
            ),
            ("run", include_str!("../tests/fixtures/go-master/run.yml")),
        ] {
            let Err(ConfigError::Validation(errors)) = parse_str(source) else {
                return Err(io::Error::other(format!(
                    "descriptor-tracking fixture {name} was accepted without a lifecycle hook"
                ))
                .into());
            };
            assert!(errors.iter().any(|error| error.contains("lifecycle hook")));
        }
        assert!(matches!(
            parse_str(include_str!("../tests/fixtures/go-master/bad-run.yml")),
            Err(ConfigError::Validation(_))
        ));
        Ok(())
    }

    #[test]
    fn normalized_v1_emits_equivalent_strict_v2() -> Result<(), Box<dyn Error>> {
        let legacy = parse_str("cmd: /bin/sleep 5\nenv:\n  COUNT: 2\nretries: 3\n")?;
        let emitted = emit_v2(&legacy.service)?;
        let reparsed = parse_str(&emitted)?;

        assert_eq!(reparsed.source, SchemaVersion::V2);
        assert_eq!(reparsed.service, legacy.service);
        assert!(emitted.contains("version: 2"));
        Ok(())
    }

    #[test]
    fn file_parser_resolves_paths_before_daemonization() -> Result<(), Box<dyn Error>> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("tests/fixtures/v2/relative.yml");
        let base = manifest.join("tests/fixtures/v2");
        let parsed = parse_file(&source)?;

        assert_eq!(parsed.service.environment_mode, EnvironmentMode::Clear);
        assert_eq!(parsed.service.working_directory, Some(base.clone()));
        assert_eq!(
            parsed.service.command.first().map(String::as_str),
            base.join("bin/api").to_str()
        );
        assert_eq!(
            parsed.service.logging.stdout.file.file,
            Some(base.join("logs/api.log"))
        );
        assert_eq!(
            parsed.service.pid_files.main,
            Some(base.join("run/main.pid"))
        );
        assert_eq!(
            parsed
                .service
                .start_condition
                .as_ref()
                .and_then(|hook| hook.command.first())
                .map(String::as_str),
            base.join("checks/network-ready").to_str()
        );
        Ok(())
    }

    #[test]
    fn file_parser_rejects_missing_working_directory() -> Result<(), Box<dyn Error>> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("tests/fixtures/v2/relative.yml");
        let yaml = fs::read_to_string(source)?.replace(
            "working_directory: .",
            "working_directory: definitely-missing-directory",
        );
        let mut parsed = parse_str(&yaml)?;
        let base = manifest.join("tests/fixtures/v2");
        super::resolve_paths(&mut parsed.service, &base)?;
        assert!(super::validate_resolved(&parsed.service).is_err());
        Ok(())
    }

    #[test]
    fn reports_unknown_v1_fields_but_rejects_unknown_v2_fields() -> Result<(), Box<dyn Error>> {
        let legacy = parse_str("cmd: /bin/true\nfuture: value\n")?;
        assert!(
            legacy
                .warnings
                .iter()
                .any(|warning| warning.path.as_deref() == Some("future"))
        );

        let modern = parse_str("version: 2\ncommand: [/bin/true]\nfuture: value\n");
        assert!(matches!(modern, Err(ConfigError::Parse(_))));
        Ok(())
    }

    #[test]
    fn parses_strict_v2_restart_and_readiness_policy() -> Result<(), Box<dyn Error>> {
        let parsed = parse_str(
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

        assert_eq!(parsed.source, SchemaVersion::V2);
        assert_eq!(parsed.service.restart.policy, RestartPolicy::OnFailure);
        assert_eq!(parsed.service.restart.limits.max_retries, Some(10));
        assert!(parsed.service.restart.exit_when_done);
        assert!(parsed.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn start_condition_has_independent_typed_backoff() -> Result<(), Box<dyn Error>> {
        let parsed = parse_str(
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
        let condition = parsed
            .service
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
    fn rejects_unsupported_versions_and_multiple_documents() {
        assert!(matches!(
            parse_str("version: 3\ncommand: [/bin/true]\n"),
            Err(ConfigError::UnsupportedVersion(3))
        ));
        assert!(parse_str("cmd: /bin/true\n---\ncmd: /bin/false\n").is_err());
    }

    #[test]
    fn rejects_duplicate_keys_excessive_depth_and_aliases() {
        assert!(matches!(
            parse_str("cmd: /bin/true\ncmd: /bin/false\n"),
            Err(ConfigError::Parse(_))
        ));

        let mut deeply_nested = "cmd: /bin/true\nfuture: ".to_owned();
        for _ in 0..40 {
            deeply_nested.push('[');
        }
        deeply_nested.push_str("value");
        for _ in 0..40 {
            deeply_nested.push(']');
        }
        assert!(matches!(
            parse_str(&deeply_nested),
            Err(ConfigError::Parse(_))
        ));

        let mut aliases = "cmd: /bin/true\nanchor: &value item\nfuture: [".to_owned();
        for position in 0..129 {
            if position != 0 {
                aliases.push_str(", ");
            }
            aliases.push_str("*value");
        }
        aliases.push_str("]\n");
        assert!(matches!(parse_str(&aliases), Err(ConfigError::Parse(_))));
    }

    #[test]
    fn rejects_oversized_and_non_utf8_documents() {
        let oversized = vec![b'a'; MAX_CONFIG_BYTES + 1];
        assert!(matches!(
            parse_bytes(&oversized),
            Err(ConfigError::TooLarge { .. })
        ));
        assert!(matches!(parse_bytes(&[0xff]), Err(ConfigError::Parse(_))));
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
        assert!(errors.iter().any(|error| error.contains("lifecycle hook")));
        Ok(())
    }

    #[test]
    fn legacy_follow_requires_a_hook_in_normalized_configuration() -> Result<(), Box<dyn Error>> {
        let result = parse_str("cmd: /bin/true\npid:\n  follow: /tmp/service.pid\n");
        assert!(matches!(result, Err(ConfigError::Validation(_))));

        let parsed = parse_str(
            "cmd: /bin/true\npost_exit: service stop\npid:\n  follow: /tmp/service.pid\n",
        )?;
        assert_eq!(parsed.service.process_mode, ProcessMode::DescriptorTracking);
        Ok(())
    }
}
