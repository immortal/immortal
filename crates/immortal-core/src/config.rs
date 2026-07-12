//! Strict service configuration types, parsing, validation, and resolution.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File},
    io::{self, Read},
    path::{Component, Path, PathBuf},
};

use serde::{Deserialize, Serialize, de::IgnoredAny};

/// Maximum accepted size of one service definition.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;

const DEFAULT_BACKOFF_INITIAL_SECONDS: u64 = 1;
const DEFAULT_BACKOFF_MAX_SECONDS: u64 = 60;
const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;
const DEFAULT_BACKOFF_JITTER_PERCENT: u8 = 20;
const DEFAULT_BACKOFF_RESET_SECONDS: u64 = 60;
const DEFAULT_READINESS_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_CONDITION_BACKOFF_MAX_SECONDS: u64 = 30;
const MAX_OPERATION_SECONDS: u64 = 86_400;
const MAX_SCHEDULE_SECONDS: u64 = 31_536_000;

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
    /// callers may then apply typed CLI overrides and call [`resolve_paths`].
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

/// Retry policy shared by independently supervised logger stages.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggerRestartConfig {
    /// Restarts permitted after the initial logger start; absent means unbounded.
    pub max_retries: Option<u32>,
    /// Delay and stable-runtime reset policy for consecutive logger failures.
    pub backoff: BackoffConfig,
}

/// Output configuration for both child streams.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct LoggingConfig {
    /// Optional external file-adapter executable; defaults to sibling `immortallog`.
    pub file_adapter: Option<PathBuf>,
    /// Route stderr through the stdout pipeline instead of creating a second pipe.
    pub combine_stderr: bool,
    /// Independent logger-stage restart and exhaustion policy.
    pub restart: LoggerRestartConfig,
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
    /// A descriptor remains open across the application's own daemonization.
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
    /// The required configuration version marker was absent.
    MissingVersion,
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
            Self::MissingVersion => formatter.write_str(
                "configuration must declare `version: 2`; unversioned Go configuration is not supported",
            ),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported configuration version {version}; only version 2 is accepted"
                )
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
            | Self::MissingVersion
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
pub fn parse_file(path: &Path) -> Result<ServiceConfig, ConfigError> {
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
    parse_bytes_at(&bytes, path)
}

/// Parse one in-memory definition using the source path for stable resolution.
///
/// # Errors
///
/// Returns the same parse, validation, resolution, and filesystem errors as
/// [`parse_file`], without reopening a path already read by a safe scanner.
pub(crate) fn parse_bytes_at(bytes: &[u8], path: &Path) -> Result<ServiceConfig, ConfigError> {
    let mut config = parse_bytes(bytes)?;
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
    resolve_paths(&mut config, base)?;
    validate_resolved(&config)?;
    Ok(config)
}

/// Parse, normalize, and validate an in-memory service definition.
///
/// # Errors
///
/// Returns an error for oversized, non-UTF-8, malformed, unsupported, or
/// semantically invalid input.
pub fn parse_bytes(bytes: &[u8]) -> Result<ServiceConfig, ConfigError> {
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
pub fn parse_str(source: &str) -> Result<ServiceConfig, ConfigError> {
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
    let config = match header.version {
        None => return Err(ConfigError::MissingVersion),
        Some(2) => parse_current(source)?,
        Some(version) => return Err(ConfigError::UnsupportedVersion(version)),
    };
    validate(&config)?;
    Ok(config)
}

/// Serialize a validated configuration using the single supported schema.
///
/// # Errors
///
/// Returns an error if the normalized model cannot be represented by the YAML
/// serializer.
pub fn emit_config(config: &ServiceConfig) -> Result<String, ConfigError> {
    #[derive(Serialize)]
    struct ConfigOutput<'a> {
        version: u8,
        #[serde(flatten)]
        service: &'a ServiceConfig,
    }

    serde_saphyr::to_string(&ConfigOutput {
        version: 2,
        service: config,
    })
    .map_err(|error| ConfigError::Parse(format!("unable to emit configuration: {error}")))
}

#[derive(Deserialize)]
struct VersionHeader {
    version: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigDocument {
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
    descriptor_tracking: Option<DescriptorTrackingConfig>,
}

fn default_true() -> bool {
    true
}

fn parse_current(source: &str) -> Result<ServiceConfig, ConfigError> {
    let document: ConfigDocument = serde_saphyr::from_str_with_options(source, yaml_options())
        .map_err(|error| ConfigError::Parse(error.to_string()))?;
    debug_assert_eq!(document.version, 2);
    Ok(ServiceConfig {
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
        descriptor_tracking: document.descriptor_tracking,
    })
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
    validate_output(&config.logging.stdout, "logging.stdout", &mut errors);
    validate_output(&config.logging.stderr, "logging.stderr", &mut errors);
    validate_backoff(
        &config.logging.restart.backoff,
        "logging.restart.backoff",
        &mut errors,
    );
    validate_path(
        config.logging.file_adapter.as_deref(),
        "logging.file_adapter",
        &mut errors,
    );
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
    if let Some(tracking) = &mut config.descriptor_tracking {
        resolve_argv_executable(&mut tracking.stop.command, &base)?;
        resolve_argv_executable(&mut tracking.reload.command, &base)?;
    }
    resolve_optional_path(&mut config.logging.file_adapter, &base);
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
    use std::{error::Error, fs, io, path::Path};

    use super::{
        ConfigError, EnvironmentMode, MAX_CONFIG_BYTES, ProcessMode, RestartPolicy, emit_config,
        parse_bytes, parse_file, parse_str, resolve_paths,
    };

    #[test]
    fn supported_configuration_round_trips() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/sleep, '5']\nenvironment:\n  COUNT: '2'\nrestart:\n  limits:\n    max_retries: 3\n",
        )?;
        let emitted = emit_config(&config)?;
        let reparsed = parse_str(&emitted)?;

        assert_eq!(reparsed, config);
        assert!(emitted.contains("version: 2"));
        Ok(())
    }

    #[test]
    fn file_parser_resolves_paths_before_daemonization() -> Result<(), Box<dyn Error>> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let source = manifest.join("tests/fixtures/v2/relative.yml");
        let base = manifest.join("tests/fixtures/v2");
        let config = parse_file(&source)?;

        assert_eq!(config.environment_mode, EnvironmentMode::Clear);
        assert_eq!(config.working_directory, Some(base.clone()));
        assert_eq!(
            config.command.first().map(String::as_str),
            base.join("bin/api").to_str()
        );
        assert_eq!(
            config.logging.stdout.file.file,
            Some(base.join("logs/api.log"))
        );
        assert_eq!(
            config.logging.file_adapter,
            Some(base.join("bin/immortallog"))
        );
        assert_eq!(config.pid_files.main, Some(base.join("run/main.pid")));
        assert_eq!(
            config
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
        let mut config = parse_str(&yaml)?;
        let base = manifest.join("tests/fixtures/v2");
        super::resolve_paths(&mut config, &base)?;
        assert!(super::validate_resolved(&config).is_err());
        Ok(())
    }

    #[test]
    fn rejects_unversioned_go_configuration_and_unknown_fields() {
        assert!(matches!(
            parse_str("cmd: /bin/true\n"),
            Err(ConfigError::MissingVersion)
        ));
        assert!(matches!(
            parse_str("version: 2\ncommand: [/bin/true]\nfuture: value\n"),
            Err(ConfigError::Parse(_))
        ));
    }

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
    fn logger_restart_policy_parses_defaults_bounds_and_unknown_fields()
    -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            r"
version: 2
command: [service]
logging:
  restart:
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
        assert_eq!(
            defaults.logging.restart.backoff,
            super::BackoffConfig::default()
        );
        assert!(matches!(
            parse_str(
                "version: 2\ncommand: [service]\nlogging:\n  restart:\n    backoff:\n      initial_seconds: 0\n"
            ),
            Err(ConfigError::Validation(_))
        ));
        assert!(matches!(
            parse_str("version: 2\ncommand: [service]\nlogging:\n  restart:\n    future: true\n"),
            Err(ConfigError::Parse(_))
        ));
        Ok(())
    }

    #[test]
    fn rejects_unsupported_versions_and_multiple_documents() {
        assert!(matches!(
            parse_str("version: 3\ncommand: [/bin/true]\n"),
            Err(ConfigError::UnsupportedVersion(3))
        ));
        assert!(
            parse_str("version: 2\ncommand: [/bin/true]\n---\nversion: 2\ncommand: [/bin/false]\n")
                .is_err()
        );
    }

    #[test]
    fn rejects_duplicate_keys_excessive_depth_and_aliases() {
        assert!(matches!(
            parse_str("version: 2\ncommand: [/bin/true]\ncommand: [/bin/false]\n"),
            Err(ConfigError::Parse(_))
        ));

        let mut deeply_nested = "version: 2\ncommand: [/bin/true]\nfuture: ".to_owned();
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

        let mut aliases =
            "version: 2\ncommand: [/bin/true]\nanchor: &value item\nfuture: [".to_owned();
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

    #[test]
    fn descriptor_tracking_hook_paths_resolve_from_definition_directory()
    -> Result<(), Box<dyn Error>> {
        let mut config = parse_str(
            "version: 2\ncommand: [/bin/true]\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [hooks/stop]\n    timeout_seconds: 1\n  reload:\n    command: [hooks/reload]\n    timeout_seconds: 1\n  lifetime_timeout_seconds: 1\n",
        )?;
        resolve_paths(&mut config, Path::new("/srv/immortal"))?;
        let tracking = config
            .descriptor_tracking
            .ok_or_else(|| io::Error::other("descriptor tracking configuration is absent"))?;
        assert_eq!(
            tracking.stop.command.first().map(String::as_str),
            Some("/srv/immortal/hooks/stop")
        );
        assert_eq!(
            tracking.reload.command.first().map(String::as_str),
            Some("/srv/immortal/hooks/reload")
        );
        Ok(())
    }
}
