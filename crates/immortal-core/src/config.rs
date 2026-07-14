//! Strict service configuration parsing, normalization, validation, and resolution.
//!
//! Version 2 wire input is converted into one typed service model before process
//! setup. Canonical logging separates local `log` routes from one combined
//! external `logger`; deprecated v2 spellings are translated only when their
//! stream behavior is representable without widening or dropping output.
//! Validation owns policy bounds and field relationships, while path resolution
//! owns definition-relative paths. Callers never need to infer precedence or
//! repair partially valid policy across a process boundary.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File, Metadata, OpenOptions},
    io::{self, BufRead, BufReader, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::{Component, Path, PathBuf},
};

use serde::{
    Deserialize, Deserializer, Serialize, Serializer,
    de::{Error as _, IgnoredAny},
    ser::SerializeMap,
};

use crate::service_name::is_safe_service_name;

/// Maximum accepted size of one service definition.
pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
/// Maximum number of entries inspected in one direct-command environment directory.
pub const MAX_ENVIRONMENT_DIRECTORY_ENTRIES: usize = 4_096;
/// Maximum accepted byte length of one environment-file first line.
pub const MAX_ENVIRONMENT_VALUE_BYTES: usize = 256 * 1024;
/// Maximum aggregate byte length of loaded environment keys and values.
pub const MAX_ENVIRONMENT_DIRECTORY_BYTES: usize = MAX_CONFIG_BYTES;

const DEFAULT_BACKOFF_INITIAL_SECONDS: u64 = 1;
const DEFAULT_BACKOFF_MAX_SECONDS: u64 = 60;
const DEFAULT_BACKOFF_MULTIPLIER: u32 = 2;
const DEFAULT_BACKOFF_JITTER_PERCENT: u8 = 20;
const DEFAULT_BACKOFF_RESET_SECONDS: u64 = 60;
const DEFAULT_READINESS_TIMEOUT_SECONDS: u64 = 30;
const DEFAULT_CONDITION_BACKOFF_MAX_SECONDS: u64 = 30;
const DEFAULT_LOG_KEEP: u32 = 7;
const MEBIBYTE: u64 = 1024 * 1024;
const MAX_OPERATION_SECONDS: u64 = 86_400;
const MAX_SCHEDULE_SECONDS: u64 = 31_536_000;
const MAX_ENVIRONMENT_VALUE_READ_BYTES: u64 = 256 * 1024 + 2;

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
    /// A direct-command environment directory or one of its entries was invalid.
    EnvironmentInput {
        /// Path which could not be inspected or decoded.
        path: PathBuf,
        /// Operating-system or bounded-input failure.
        source: io::Error,
    },
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
            Self::EnvironmentInput { path, source } => {
                write!(
                    formatter,
                    "unable to load environment input `{}`: {source}",
                    path.display()
                )
            }
            Self::Parse(error) => write!(formatter, "invalid YAML configuration: {error}"),
            Self::MissingVersion => formatter.write_str(
                "configuration must declare `version: 2`; unversioned Go configuration is intentionally unsupported; see INSTALL.md#definition-migration",
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
            Self::EnvironmentInput { source, .. } => Some(source),
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

/// Load the Go-compatible direct-command environment-directory format.
///
/// Each regular file contributes its UTF-8 filename and first UTF-8 line. A
/// physically empty file contributes nothing, while a first empty line sets an
/// empty value. CRLF is normalized like Go's line scanner. Symlinks and other
/// non-regular entries are never followed and do not contribute values.
///
/// The returned snapshot is bounded and detached from the directory, so daemon
/// startup and every later service generation use the same materialized values.
///
/// # Errors
///
/// Returns an error when the directory is absent, symlinked, not a directory,
/// changes during the scan, contains an unreadable or changing regular file, or
/// exceeds the entry, first-line, aggregate-size, UTF-8, or environment bounds.
pub fn load_environment_directory(
    directory: &Path,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let directory_before =
        fs::symlink_metadata(directory).map_err(|error| environment_error(directory, error))?;
    if directory_before.file_type().is_symlink() || !directory_before.is_dir() {
        return Err(invalid_environment_input(
            directory,
            "environment path must be a real directory, not a symlink",
        ));
    }
    let canonical =
        fs::canonicalize(directory).map_err(|error| environment_error(directory, error))?;
    let canonical_before =
        fs::metadata(&canonical).map_err(|error| environment_error(&canonical, error))?;
    if environment_directory_changed(&directory_before, &canonical_before) {
        return Err(invalid_environment_input(
            directory,
            "environment directory changed during validation",
        ));
    }

    let entries = fs::read_dir(&canonical).map_err(|error| environment_error(&canonical, error))?;
    let mut environment = BTreeMap::new();
    let mut entry_count = 0_usize;
    let mut total_bytes = 0_usize;
    for entry in entries {
        entry_count = entry_count.checked_add(1).ok_or_else(|| {
            invalid_environment_input(&canonical, "environment entry count overflowed")
        })?;
        if entry_count > MAX_ENVIRONMENT_DIRECTORY_ENTRIES {
            return Err(invalid_environment_input(
                &canonical,
                format!(
                    "environment directory has more than {MAX_ENVIRONMENT_DIRECTORY_ENTRIES} entries"
                ),
            ));
        }
        let entry = entry.map_err(|error| environment_error(&canonical, error))?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| environment_error(&path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let key = entry.file_name().into_string().map_err(|_| {
            invalid_environment_input(&path, "environment filename is not valid UTF-8")
        })?;
        if !environment_key_is_valid(&key) {
            return Err(invalid_environment_input(
                &path,
                format!("environment filename {key:?} is not a valid key"),
            ));
        }
        let Some(value) = read_environment_value(&path, &metadata)? else {
            continue;
        };
        let entry_bytes = key
            .len()
            .checked_add(value.len())
            .ok_or_else(|| invalid_environment_input(&path, "environment entry size overflowed"))?;
        total_bytes = total_bytes.checked_add(entry_bytes).ok_or_else(|| {
            invalid_environment_input(&canonical, "environment aggregate size overflowed")
        })?;
        if total_bytes > MAX_ENVIRONMENT_DIRECTORY_BYTES {
            return Err(invalid_environment_input(
                &canonical,
                format!(
                    "environment keys and values exceed {MAX_ENVIRONMENT_DIRECTORY_BYTES} bytes"
                ),
            ));
        }
        environment.insert(key, value);
    }

    let directory_after =
        fs::metadata(&canonical).map_err(|error| environment_error(&canonical, error))?;
    if environment_directory_changed(&canonical_before, &directory_after) {
        return Err(invalid_environment_input(
            &canonical,
            "environment directory changed during the scan",
        ));
    }
    Ok(environment)
}

fn read_environment_value(
    path: &Path,
    path_before: &Metadata,
) -> Result<Option<String>, ConfigError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .map_err(|error| environment_error(path, error))?;
    let file_before = file
        .metadata()
        .map_err(|error| environment_error(path, error))?;
    if environment_file_changed(path_before, &file_before) {
        return Err(invalid_environment_input(
            path,
            "environment file changed before it was opened",
        ));
    }

    let mut bytes = Vec::new();
    {
        let mut reader = BufReader::new((&mut file).take(MAX_ENVIRONMENT_VALUE_READ_BYTES));
        reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| environment_error(path, error))?;
    }

    let file_after = file
        .metadata()
        .map_err(|error| environment_error(path, error))?;
    let path_after = fs::symlink_metadata(path).map_err(|error| environment_error(path, error))?;
    if path_after.file_type().is_symlink()
        || environment_file_changed(&file_before, &file_after)
        || environment_file_changed(&file_before, &path_after)
    {
        return Err(invalid_environment_input(
            path,
            "environment file changed while it was read",
        ));
    }

    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_ENVIRONMENT_VALUE_BYTES {
        return Err(invalid_environment_input(
            path,
            format!(
                "environment first line is {} bytes; limit is {MAX_ENVIRONMENT_VALUE_BYTES}",
                bytes.len()
            ),
        ));
    }
    if bytes.contains(&0) {
        return Err(invalid_environment_input(
            path,
            "environment first line contains NUL",
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|error| {
        invalid_environment_input(
            path,
            format!("environment first line is not valid UTF-8: {error}"),
        )
    })
}

fn environment_error(path: &Path, source: io::Error) -> ConfigError {
    ConfigError::EnvironmentInput {
        path: path.to_owned(),
        source,
    }
}

fn invalid_environment_input(path: &Path, message: impl Into<String>) -> ConfigError {
    environment_error(
        path,
        io::Error::new(io::ErrorKind::InvalidData, message.into()),
    )
}

fn environment_directory_changed(before: &Metadata, after: &Metadata) -> bool {
    !same_file_identity(before, after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || !after.is_dir()
}

fn environment_file_changed(before: &Metadata, after: &Metadata) -> bool {
    !same_file_identity(before, after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || !after.is_file()
}

fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
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

    validate(config)?;
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
    #[serde(default, alias = "env", deserialize_with = "deserialize_environment")]
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
    log: Option<LogInput>,
    logger: Option<Vec<String>>,
    log_adapter: Option<PathBuf>,
    logger_restart: Option<LoggerRestartConfig>,
    /// Deprecated Go-compatible alias for `log.stderr`.
    stderr: Option<FileLogInput>,
    /// Deprecated Rust v2 logging shape accepted during migration.
    logging: Option<LegacyLoggingConfig>,
    #[serde(default)]
    pid_files: PidFiles,
    #[serde(default)]
    process_mode: ProcessMode,
    descriptor_tracking: Option<DescriptorTrackingConfig>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LogInput {
    Combined(FileLogInput),
    Selected(SelectedLogInput),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedLogInput {
    stdout: Option<FileLogInput>,
    stderr: Option<FileLogInput>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileLogInput {
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

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct LegacyLoggingConfig {
    file_adapter: Option<PathBuf>,
    combine_stderr: bool,
    restart: LoggerRestartConfig,
    stdout: LegacyOutputConfig,
    stderr: LegacyOutputConfig,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum LogScalar {
    Integer(u64),
    Text(String),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EnvironmentValue {
    Text(String),
    Signed(i64),
    Unsigned(u64),
    Float(f64),
    Boolean(bool),
}

impl EnvironmentValue {
    fn into_string(self) -> String {
        match self {
            Self::Text(value) => value,
            Self::Signed(value) => value.to_string(),
            Self::Unsigned(value) => value.to_string(),
            Self::Float(value) => value.to_string(),
            Self::Boolean(value) => value.to_string(),
        }
    }
}

fn deserialize_environment<'de, D>(deserializer: D) -> Result<BTreeMap<String, String>, D::Error>
where
    D: Deserializer<'de>,
{
    BTreeMap::<String, EnvironmentValue>::deserialize(deserializer).map(|environment| {
        environment
            .into_iter()
            .map(|(key, value)| (key, value.into_string()))
            .collect()
    })
}

fn deserialize_optional_log_age<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<LogScalar>::deserialize(deserializer)?
        .map(parse_log_age)
        .transpose()
        .map_err(D::Error::custom)
}

fn deserialize_optional_log_size<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<LogScalar>::deserialize(deserializer)?
        .map(parse_log_size)
        .transpose()
        .map_err(D::Error::custom)
}

fn parse_log_age(value: LogScalar) -> Result<u64, String> {
    match value {
        LogScalar::Integer(seconds) => positive_value(seconds, "log age"),
        LogScalar::Text(duration) => parse_compact_quantity(
            &duration,
            &[
                ("w", 7 * 24 * 60 * 60),
                ("d", 24 * 60 * 60),
                ("h", 60 * 60),
                ("m", 60),
                ("s", 1),
            ],
            "log age",
        ),
    }
}

fn parse_log_size(value: LogScalar) -> Result<u64, String> {
    match value {
        LogScalar::Integer(mebibytes) => positive_value(mebibytes, "log size")?
            .checked_mul(MEBIBYTE)
            .ok_or_else(|| "log size exceeds the supported byte range".to_owned()),
        LogScalar::Text(size) => parse_compact_quantity(
            &size,
            &[
                ("GiB", 1024 * MEBIBYTE),
                ("MiB", MEBIBYTE),
                ("KiB", 1024),
                ("B", 1),
            ],
            "log size",
        ),
    }
}

fn parse_compact_quantity(
    value: &str,
    units: &[(&str, u64)],
    description: &str,
) -> Result<u64, String> {
    let Some((digits, multiplier)) = units.iter().find_map(|(suffix, multiplier)| {
        value
            .strip_suffix(*suffix)
            .map(|digits| (digits, *multiplier))
    }) else {
        return Err(format!("{description} has an unknown or missing unit"));
    };
    if digits.is_empty()
        || !digits.as_bytes().iter().all(u8::is_ascii_digit)
        || (digits.len() > 1 && digits.as_bytes().first() == Some(&b'0'))
    {
        return Err(format!("{description} must use a positive compact integer"));
    }
    let quantity = digits
        .parse::<u64>()
        .map_err(|_| format!("{description} exceeds the supported numeric range"))?;
    positive_value(quantity, description)?
        .checked_mul(multiplier)
        .ok_or_else(|| format!("{description} exceeds the supported numeric range"))
}

fn positive_value(value: u64, description: &str) -> Result<u64, String> {
    if value == 0 {
        Err(format!("{description} must be greater than zero"))
    } else {
        Ok(value)
    }
}

fn format_log_age(seconds: u64) -> String {
    format_quantity(
        seconds,
        &[
            ("w", 7 * 24 * 60 * 60),
            ("d", 24 * 60 * 60),
            ("h", 60 * 60),
            ("m", 60),
            ("s", 1),
        ],
    )
}

fn format_log_size(bytes: u64) -> String {
    format_quantity(
        bytes,
        &[
            ("GiB", 1024 * MEBIBYTE),
            ("MiB", MEBIBYTE),
            ("KiB", 1024),
            ("B", 1),
        ],
    )
}

fn format_quantity(value: u64, units: &[(&str, u64)]) -> String {
    units
        .iter()
        .find(|(_, multiplier)| value >= *multiplier && value.is_multiple_of(*multiplier))
        .map_or_else(
            || value.to_string(),
            |(suffix, multiplier)| format!("{}{suffix}", value / multiplier),
        )
}

fn logger_restart_is_default(restart: &LoggerRestartConfig) -> bool {
    restart == &LoggerRestartConfig::default()
}

fn default_true() -> bool {
    true
}

fn parse_current(source: &str) -> Result<ServiceConfig, ConfigError> {
    let document: ConfigDocument = serde_saphyr::from_str_with_options(source, yaml_options())
        .map_err(|error| ConfigError::Parse(error.to_string()))?;
    debug_assert_eq!(document.version, 2);
    let ConfigDocument {
        version: _,
        enabled,
        command,
        working_directory,
        environment,
        environment_mode,
        user,
        start_delay_seconds,
        restart,
        readiness,
        requires,
        start_condition,
        post_exit,
        log,
        logger,
        log_adapter,
        logger_restart,
        stderr,
        logging,
        pid_files,
        process_mode,
        descriptor_tracking,
    } = document;
    let logging = normalize_logging(log, logger, log_adapter, logger_restart, stderr, logging)?;
    Ok(ServiceConfig {
        enabled,
        command,
        working_directory,
        environment,
        environment_mode,
        user,
        start_delay_seconds,
        restart,
        readiness,
        requires,
        start_condition,
        post_exit,
        logging,
        pid_files,
        process_mode,
        descriptor_tracking,
    })
}

fn normalize_logging(
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
        Some(DEFAULT_LOG_KEEP)
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

fn environment_key_is_valid(key: &str) -> bool {
    !key.is_empty() && !key.contains('=') && !key.contains('\0')
}

fn environment_value_is_valid(value: &str) -> bool {
    !value.contains('\0')
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
    if let Some(files) = &mut config.logging.files {
        match files {
            FileLogRoutes::Combined(file) => resolve_file_log_path(file, &base),
            FileLogRoutes::Selected { stdout, stderr } => {
                if let Some(file) = stdout {
                    resolve_file_log_path(file, &base);
                }
                if let Some(file) = stderr {
                    resolve_file_log_path(file, &base);
                }
            }
        }
    }
    if let Some(logger) = &mut config.logging.logger {
        resolve_argv_executable(logger, &base)?;
    }
    resolve_optional_path(&mut config.pid_files.supervisor, &base);
    resolve_optional_path(&mut config.pid_files.main, &base);
    Ok(())
}

fn resolve_file_log_path(file: &mut FileLogConfig, base: &Path) {
    file.file = normalize_absolute(base, &file.file);
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

fn validate_file_log(file: &FileLogConfig, path: &str, errors: &mut Vec<String>) {
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
    use std::{
        collections::{BTreeMap, BTreeSet},
        error::Error,
        fs, io,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        BackoffConfig, CommandHook, ConditionBackoffConfig, ConfigError, DescriptorTrackingConfig,
        EnvironmentMode, FileLogConfig, FileLogRoutes, LoggerRestartConfig, LoggingConfig,
        MAX_CONFIG_BYTES, MAX_ENVIRONMENT_DIRECTORY_BYTES, MAX_ENVIRONMENT_DIRECTORY_ENTRIES,
        MAX_ENVIRONMENT_VALUE_BYTES, PidFiles, ProcessMode, ReadinessConfig, ReadinessMode,
        RestartBurstLimit, RestartConfig, RestartLimits, RestartPolicy, ServiceConfig,
        StartConditionConfig, emit_config, load_environment_directory, parse_bytes, parse_file,
        parse_str, resolve_paths,
    };

    #[test]
    fn supported_configuration_round_trips() -> Result<(), Box<dyn Error>> {
        let config = parse_str(include_str!("../tests/fixtures/v2/complete.yml"))?;
        let expected = complete_config();
        assert_eq!(config, expected);

        let emitted = emit_config(&config)?;
        let reparsed = parse_str(&emitted)?;

        assert_eq!(reparsed, config);
        assert!(emitted.contains("version: 2"));
        Ok(())
    }

    #[test]
    fn env_alias_normalizes_scalars_and_emits_canonical_environment() -> Result<(), Box<dyn Error>>
    {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: 1\n  ENVIRONMENT: production\n  ENABLED: true\n",
        )?;
        assert_eq!(
            config.environment,
            BTreeMap::from([
                ("DEBUG".to_owned(), "1".to_owned()),
                ("ENABLED".to_owned(), "true".to_owned()),
                ("ENVIRONMENT".to_owned(), "production".to_owned()),
            ])
        );

        let emitted = emit_config(&config)?;
        assert!(emitted.lines().any(|line| line == "environment:"));
        assert!(!emitted.lines().any(|line| line == "env:"));
        Ok(())
    }

    #[test]
    fn env_alias_rejects_duplicate_and_non_scalar_values() {
        let duplicate = parse_str(
            "version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: 1\nenvironment:\n  MODE: test\n",
        );
        assert!(duplicate.is_err());

        let nested = parse_str("version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: [one, two]\n");
        assert!(nested.is_err());
        let null = parse_str("version: 2\ncommand: [/bin/true]\nenv:\n  DEBUG: null\n");
        assert!(null.is_err());
    }

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
    fn logging_duration_and_size_units_are_checked_and_canonical() -> Result<(), Box<dyn Error>> {
        for (value, expected) in [
            ("1s", 1),
            ("2m", 120),
            ("3h", 10_800),
            ("4d", 345_600),
            ("2w", 1_209_600),
        ] {
            let source = format!(
                "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: {value}\n"
            );
            let config = parse_str(&source)?;
            let Some(FileLogRoutes::Combined(file)) = config.logging.files else {
                return Err("combined age route is missing".into());
            };
            assert_eq!(file.max_age_seconds, Some(expected));
            assert_eq!(file.keep, Some(7));
        }

        for (value, expected) in [
            ("1B", 1),
            ("2KiB", 2_048),
            ("3MiB", 3_145_728),
            ("4GiB", 4_294_967_296),
            ("2", 2_097_152),
        ] {
            let source = format!(
                "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  size: {value}\n"
            );
            let config = parse_str(&source)?;
            let Some(FileLogRoutes::Combined(file)) = config.logging.files else {
                return Err("combined size route is missing".into());
            };
            assert_eq!(file.max_bytes, Some(expected));
            assert_eq!(file.keep, Some(7));
        }

        let canonical = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/app.log\n  age: 3600\n  size: 1024KiB\n",
        )?;
        let emitted = emit_config(&canonical)?;
        assert!(emitted.contains("age: 1h"));
        assert!(emitted.contains("size: 1MiB"));
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

    #[test]
    fn environment_directory_loads_regular_file_first_lines() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("environment-load")?;
        fs::write(directory.join("DEBUG"), "true\nignored\n")?;
        fs::write(directory.join("ENVIRONMENT"), "production\r\nignored\n")?;
        fs::write(directory.join("EMPTY"), "\nignored\n")?;
        fs::write(directory.join("ABSENT"), "")?;
        fs::create_dir(directory.join("nested"))?;
        symlink(directory.join("DEBUG"), directory.join("LINK"))?;

        let environment = load_environment_directory(directory.path())?;
        assert_eq!(
            environment,
            BTreeMap::from([
                ("DEBUG".to_owned(), "true".to_owned()),
                ("EMPTY".to_owned(), String::new()),
                ("ENVIRONMENT".to_owned(), "production".to_owned()),
            ])
        );
        Ok(())
    }

    #[test]
    fn environment_directory_rejects_unsafe_or_malformed_inputs() -> Result<(), Box<dyn Error>> {
        let parent = TestDirectory::new("environment-invalid")?;
        let directory = parent.join("actual");
        fs::create_dir(&directory)?;
        let link = parent.join("link");
        symlink(&directory, &link)?;
        assert!(load_environment_directory(&link).is_err());
        assert!(load_environment_directory(&parent.join("missing")).is_err());

        let invalid_key = directory.join("BAD=KEY");
        fs::write(&invalid_key, "value\n")?;
        assert!(load_environment_directory(&directory).is_err());
        fs::remove_file(&invalid_key)?;

        let invalid_value = directory.join("INVALID_UTF8");
        fs::write(&invalid_value, [0xff, b'\n'])?;
        assert!(load_environment_directory(&directory).is_err());
        fs::remove_file(&invalid_value)?;

        let oversized = directory.join("OVERSIZED");
        fs::write(&oversized, vec![b'x'; MAX_ENVIRONMENT_VALUE_BYTES + 1])?;
        assert!(load_environment_directory(&directory).is_err());
        Ok(())
    }

    #[test]
    fn environment_directory_enforces_entry_and_aggregate_bounds() -> Result<(), Box<dyn Error>> {
        let entries = TestDirectory::new("environment-entry-bound")?;
        for index in 0..=MAX_ENVIRONMENT_DIRECTORY_ENTRIES {
            fs::write(entries.join(format!("ENTRY_{index}")), "")?;
        }
        assert!(load_environment_directory(entries.path()).is_err());

        let aggregate = TestDirectory::new("environment-aggregate-bound")?;
        let value = vec![b'x'; MAX_ENVIRONMENT_VALUE_BYTES];
        let files = MAX_ENVIRONMENT_DIRECTORY_BYTES
            .checked_div(MAX_ENVIRONMENT_VALUE_BYTES)
            .and_then(|count| count.checked_add(1))
            .ok_or("environment test bound overflowed")?;
        for index in 0..files {
            fs::write(aggregate.join(format!("VALUE_{index}")), &value)?;
        }
        assert!(load_environment_directory(aggregate.path()).is_err());
        Ok(())
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "immortal-config-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
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
        let Some(FileLogRoutes::Selected {
            stdout: Some(stdout),
            ..
        }) = &config.logging.files
        else {
            return Err("relative stdout log route is missing".into());
        };
        assert_eq!(stdout.file, base.join("logs/api.log"));
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
    fn schema_selection_requires_explicit_version_two() {
        for legacy in [
            "cmd: /bin/true\n",
            "cmd: /bin/true\npid:\n  follow: /run/service.pid\n",
            "command: [/bin/true]\nrestart:\n  policy: never\n",
            "cmd: /bin/true\ncommand: [/bin/false]\n",
        ] {
            assert!(matches!(
                parse_str(legacy),
                Err(ConfigError::MissingVersion)
            ));
        }
        assert!(parse_str("cmd: /bin/true\n").is_err_and(|error| {
            error
                .to_string()
                .contains("INSTALL.md#definition-migration")
        }));
        assert!(matches!(
            parse_str("version: 1\ncmd: /bin/true\n"),
            Err(ConfigError::UnsupportedVersion(1))
        ));
        assert!(matches!(
            parse_str("version: 3\ncommand: [/bin/true]\n"),
            Err(ConfigError::UnsupportedVersion(3))
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
        assert_eq!(
            defaults.logging.restart.backoff,
            super::BackoffConfig::default()
        );
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

    #[test]
    fn rejects_unsupported_versions_and_multiple_documents() {
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
