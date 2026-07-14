//! Local-file routing, shared external-logger planning, and file rotation.
//!
//! Configuration normalizes into strict local routes plus at most one external
//! sink. Combined routes use one file adapter; selected routes keep stdout and
//! stderr independent locally while both may converge on the shared sink.
//! This module models that ownership and lifecycle but never copies service
//! bytes: the broker materializes stable pipes and kernel backpressure remains
//! lossless. The rotating writer separately owns sync-before-rename, archive
//! retention, timestamping, and bounded-memory partial-line handling.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::{FileLogConfig, FileLogRoutes, LoggingConfig},
    supervisor::SupervisorState,
};

/// Child output stream attached to one pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    /// Standard output only.
    Stdout,
    /// Standard error only.
    Stderr,
    /// Standard output and error share one pipe.
    Combined,
}

/// Backpressure behavior between a service and logger chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackpressurePolicy {
    /// Block producers through normal kernel-pipe pressure; never drop silently.
    LosslessBlock,
}

/// One separately supervised process in a logger chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoggerStage {
    /// Small replaceable `immortallog` compatibility process.
    FileAdapter(FileLogConfig),
    /// Operator-selected external logger argv.
    External(Vec<String>),
}

/// One local-file route attached to an exact child stream selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRoutePlan {
    /// Service descriptors connected to this adapter.
    pub stream: OutputStream,
    /// Local destination and rotation policy.
    pub file: FileLogConfig,
    /// Loss policy between processes.
    pub backpressure: BackpressurePolicy,
}

/// Complete local-file and centralized-logger graph for one service.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoggingPlan {
    /// Zero, one, or two independently supervised local file adapters.
    pub local_files: Vec<FileRoutePlan>,
    /// At most one external logger receiving merged stdout and stderr.
    pub logger: Option<Vec<String>>,
}

impl LoggingPlan {
    /// Normalize output configuration into local routes and one shared sink.
    ///
    /// Local stream selection never filters the external logger. When both are
    /// configured, file-adapter passthrough writers converge on one kernel pipe
    /// owned by the broker.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty external command or empty selected route.
    pub fn from_config(config: &LoggingConfig) -> Result<Self, LoggingError> {
        if config
            .logger
            .as_ref()
            .is_some_and(|command| command.first().is_none_or(String::is_empty))
        {
            return Err(LoggingError::EmptyExternalCommand);
        }

        let mut local_files = Vec::new();
        match &config.files {
            Some(FileLogRoutes::Combined(file)) => local_files.push(FileRoutePlan {
                stream: OutputStream::Combined,
                file: file.clone(),
                backpressure: BackpressurePolicy::LosslessBlock,
            }),
            Some(FileLogRoutes::Selected { stdout, stderr }) => {
                if stdout.is_none() && stderr.is_none() {
                    return Err(LoggingError::EmptyFileSelection);
                }
                if let Some(file) = stdout {
                    local_files.push(FileRoutePlan {
                        stream: OutputStream::Stdout,
                        file: file.clone(),
                        backpressure: BackpressurePolicy::LosslessBlock,
                    });
                }
                if let Some(file) = stderr {
                    local_files.push(FileRoutePlan {
                        stream: OutputStream::Stderr,
                        file: file.clone(),
                        backpressure: BackpressurePolicy::LosslessBlock,
                    });
                }
            }
            None => {}
        }
        Ok(Self {
            local_files,
            logger: config.logger.clone(),
        })
    }
}

/// Durable identity for stable pipe endpoints, independent of child PIDs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PipeId(u64);

impl PipeId {
    /// Numeric identity exposed in diagnostics.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Health summarized across every separately supervised logger stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineHealth {
    /// Every stage is ready to consume bytes.
    Ready,
    /// At least one stage is starting or stopped.
    Starting,
    /// At least one stage is waiting in restart backoff.
    Backoff,
    /// At least one stage exhausted its configured restart policy.
    Failed,
}

/// Runtime status for one logger process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggerStageStatus {
    /// Stage definition.
    pub stage: LoggerStage,
    /// Stage-local logical generation.
    pub generation: u64,
    /// Observed child lifecycle.
    pub state: SupervisorState,
}

/// Runtime ownership of stable pipes and restartable logger stages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggingRuntime {
    local_files: Vec<FileRouteRuntime>,
    logger: Option<SharedLoggerRuntime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileRouteRuntime {
    plan: FileRoutePlan,
    pipe: PipeId,
    stage: LoggerStageStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SharedLoggerRuntime {
    pipe: PipeId,
    stage: LoggerStageStatus,
}

impl LoggingRuntime {
    /// Allocate durable pipe identities before starting any logger or service.
    #[must_use]
    pub fn new(plan: LoggingPlan) -> Self {
        let logger_position = plan.local_files.len();
        let local_files = plan
            .local_files
            .into_iter()
            .enumerate()
            .map(|(position, plan)| {
                let stage = LoggerStageStatus {
                    stage: LoggerStage::FileAdapter(plan.file.clone()),
                    generation: 0,
                    state: SupervisorState::Down,
                };
                FileRouteRuntime {
                    pipe: PipeId(
                        u64::try_from(position)
                            .unwrap_or(u64::MAX)
                            .saturating_add(1),
                    ),
                    plan,
                    stage,
                }
            })
            .collect();
        let logger = plan.logger.map(|command| SharedLoggerRuntime {
            pipe: PipeId(
                u64::try_from(logger_position)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            ),
            stage: LoggerStageStatus {
                stage: LoggerStage::External(command),
                generation: 0,
                state: SupervisorState::Down,
            },
        });
        Self {
            local_files,
            logger,
        }
    }

    /// Stable service-input pipe identity for an output stream.
    #[must_use]
    pub fn pipe(&self, stream: OutputStream) -> Option<PipeId> {
        self.local_files
            .iter()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .map(|route| route.pipe)
            .or_else(|| self.logger.as_ref().map(|logger| logger.pipe))
    }

    /// Stable shared pipe feeding the one external logger.
    #[must_use]
    pub fn logger_pipe(&self) -> Option<PipeId> {
        self.logger.as_ref().map(|logger| logger.pipe)
    }

    /// Replace one file-adapter generation/state without replacing its pipe.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown stream or a nonzero stage index.
    pub fn update_stage(
        &mut self,
        stream: OutputStream,
        stage_index: usize,
        generation: u64,
        lifecycle_state: SupervisorState,
    ) -> Result<(), LoggingError> {
        if stage_index != 0 {
            return Err(LoggingError::UnknownStage);
        }
        let route = self
            .local_files
            .iter_mut()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .ok_or(LoggingError::UnknownPipeline)?;
        route.stage.generation = generation;
        route.stage.state = lifecycle_state;
        Ok(())
    }

    /// Replace the shared external logger state without replacing its pipe.
    ///
    /// # Errors
    ///
    /// Returns an error when no external logger is configured.
    pub fn update_logger(
        &mut self,
        generation: u64,
        lifecycle_state: SupervisorState,
    ) -> Result<(), LoggingError> {
        let logger = self
            .logger
            .as_mut()
            .ok_or(LoggingError::UnknownExternalLogger)?;
        logger.stage.generation = generation;
        logger.stage.state = lifecycle_state;
        Ok(())
    }

    /// Aggregate health for one stream's local route and shared logger.
    #[must_use]
    pub fn health(&self, stream: OutputStream) -> Option<PipelineHealth> {
        let local = self
            .local_files
            .iter()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .map(|route| &route.stage);
        let logger = self.logger.as_ref().map(|logger| &logger.stage);
        let stages: Vec<&LoggerStageStatus> = local.into_iter().chain(logger).collect();
        (!stages.is_empty()).then(|| summarize_health(&stages))
    }

    /// Immutable stage statuses affecting one stream.
    #[must_use]
    pub fn stages(&self, stream: OutputStream) -> Vec<&LoggerStageStatus> {
        let local = self
            .local_files
            .iter()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .map(|route| &route.stage);
        local
            .into_iter()
            .chain(self.logger.as_ref().map(|logger| &logger.stage))
            .collect()
    }
}

fn route_matches_stream(route: OutputStream, stream: OutputStream) -> bool {
    route == stream
        || matches!(
            (route, stream),
            (
                OutputStream::Combined,
                OutputStream::Stdout | OutputStream::Stderr
            )
        )
}

fn summarize_health(stages: &[&LoggerStageStatus]) -> PipelineHealth {
    if stages
        .iter()
        .any(|stage| matches!(stage.state, SupervisorState::Failed(_)))
    {
        PipelineHealth::Failed
    } else if stages
        .iter()
        .any(|stage| matches!(stage.state, SupervisorState::Backoff { .. }))
    {
        PipelineHealth::Backoff
    } else if stages
        .iter()
        .all(|stage| matches!(stage.state, SupervisorState::Ready(_)))
    {
        PipelineHealth::Ready
    } else {
        PipelineHealth::Starting
    }
}

/// Ordered shutdown phase for service and logging processes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoggingShutdownPhase {
    /// Normal operation.
    #[default]
    Running,
    /// Service group must stop before pipe writers are closed.
    StoppingService,
    /// Service is reaped; logger chain drains remaining pipe bytes.
    Draining,
    /// Drain completed or timed out; logger stages may stop downstream-first.
    StoppingLoggers,
    /// Every logging child has been reaped.
    Complete,
}

/// Work selected by one shutdown transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggingShutdownEffect {
    /// Stop the service process group.
    StopService,
    /// Close service-side pipe writers and wait for logger drain.
    BeginDrain,
    /// Stop logger processes from final consumer back toward the service.
    StopLoggersDownstreamFirst,
    /// Shutdown is complete.
    Complete,
}

/// Enforces service-before-drain-before-logger shutdown ordering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoggingShutdown {
    phase: LoggingShutdownPhase,
}

impl LoggingShutdown {
    /// Current externally reportable phase.
    #[must_use]
    pub const fn phase(self) -> LoggingShutdownPhase {
        self.phase
    }

    /// Start shutdown, accounting for a service which may already be absent.
    ///
    /// # Errors
    ///
    /// Returns an error unless shutdown is starting from normal operation.
    pub fn begin(&mut self, service_is_live: bool) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::Running {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        if service_is_live {
            self.phase = LoggingShutdownPhase::StoppingService;
            Ok(LoggingShutdownEffect::StopService)
        } else {
            self.phase = LoggingShutdownPhase::Draining;
            Ok(LoggingShutdownEffect::BeginDrain)
        }
    }

    /// Record complete service-group reaping and begin drain.
    ///
    /// # Errors
    ///
    /// Returns an error unless the service was being stopped.
    pub fn service_stopped(&mut self) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::StoppingService {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        self.phase = LoggingShutdownPhase::Draining;
        Ok(LoggingShutdownEffect::BeginDrain)
    }

    /// Record successful drain or a bounded drain timeout.
    ///
    /// # Errors
    ///
    /// Returns an error unless bytes are currently draining.
    pub fn drain_finished(&mut self) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::Draining {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        self.phase = LoggingShutdownPhase::StoppingLoggers;
        Ok(LoggingShutdownEffect::StopLoggersDownstreamFirst)
    }

    /// Record complete logger reaping.
    ///
    /// # Errors
    ///
    /// Returns an error unless logger shutdown is in progress.
    pub fn loggers_stopped(&mut self) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::StoppingLoggers {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        self.phase = LoggingShutdownPhase::Complete;
        Ok(LoggingShutdownEffect::Complete)
    }
}

/// Invalid logger plan, runtime lookup, or shutdown transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggingError {
    /// External logger argv has no executable.
    EmptyExternalCommand,
    /// Selected local-file routing has no stream.
    EmptyFileSelection,
    /// Runtime stream does not have a pipeline.
    UnknownPipeline,
    /// Pipeline stage index does not exist.
    UnknownStage,
    /// Runtime has no shared external logger.
    UnknownExternalLogger,
    /// Shutdown event arrived out of order.
    InvalidShutdownTransition,
    /// Archive count cannot fit the target platform.
    RetentionCountTooLarge,
}

impl Display for LoggingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExternalCommand => formatter.write_str("external logger command is empty"),
            Self::EmptyFileSelection => formatter.write_str("local log selection is empty"),
            Self::UnknownPipeline => formatter.write_str("unknown logging pipeline"),
            Self::UnknownStage => formatter.write_str("unknown logger stage"),
            Self::UnknownExternalLogger => formatter.write_str("external logger is not configured"),
            Self::InvalidShutdownTransition => {
                formatter.write_str("invalid logging shutdown transition")
            }
            Self::RetentionCountTooLarge => {
                formatter.write_str("logging retention count is too large")
            }
        }
    }
}

impl Error for LoggingError {}

/// Rotation and retention policy used by the external file adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RotationPolicy {
    /// Rotate before a write would exceed this many bytes.
    pub max_bytes: Option<u64>,
    /// Rotate a nonempty file after this elapsed age.
    pub max_age: Option<Duration>,
    /// Maximum number of Immortal-owned archives.
    pub keep: Option<usize>,
    /// Maximum combined bytes across Immortal-owned archives.
    pub max_total_bytes: Option<u64>,
}

impl RotationPolicy {
    /// Convert validated service configuration into adapter limits.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive count cannot fit the target platform's
    /// address space.
    pub fn from_config(config: &FileLogConfig) -> Result<Self, LoggingError> {
        Ok(Self {
            max_bytes: config.max_bytes,
            max_age: config.max_age_seconds.map(Duration::from_secs),
            keep: config
                .keep
                .map(usize::try_from)
                .transpose()
                .map_err(|_| LoggingError::RetentionCountTooLarge)?,
            max_total_bytes: None,
        })
    }
}

/// Append-only file sink with atomic rename rotation and bounded retention.
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    policy: RotationPolicy,
    bytes_written: u64,
    opened_at: SystemTime,
    archive_sequence: u64,
}

impl RotatingFile {
    /// Open or create a destination and enforce existing archive retention.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid destination, open/metadata failure, or
    /// inability to enforce retention.
    pub fn open(path: &Path, policy: RotationPolicy) -> io::Result<Self> {
        if path.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log destination must have a filename",
            ));
        }
        validate_rotation_policy(policy)?;
        let file = open_append(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log destination is not a regular file",
            ));
        }
        let mut sink = Self {
            path: path.to_owned(),
            file,
            policy,
            bytes_written: metadata.len(),
            opened_at: metadata.modified().unwrap_or_else(|_| SystemTime::now()),
            archive_sequence: 0,
        };
        sink.enforce_retention()?;
        Ok(sink)
    }

    /// Write an entire byte slice, rotating once before it when required.
    ///
    /// # Errors
    ///
    /// Returns an error for rotation, retention, or write failure. Partial
    /// writes are never reported as success.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_parts(&[bytes])
    }

    /// Write multiple parts as one rotation unit.
    ///
    /// This keeps a timestamp prefix and its record in the same archive without
    /// buffering arbitrarily large lines.
    ///
    /// # Errors
    ///
    /// Returns an error for length overflow, rotation, retention, or write failure.
    pub fn write_parts(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        let incoming = parts.iter().try_fold(0_u64, |total, part| {
            u64::try_from(part.len())
                .ok()
                .and_then(|length| total.checked_add(length))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "log write is too large")
                })
        })?;
        self.rotate_if_needed(incoming)?;
        for part in parts {
            self.file.write_all(part)?;
        }
        self.bytes_written = self.bytes_written.saturating_add(incoming);
        Ok(())
    }

    /// Flush userspace buffers and request durable file contents.
    ///
    /// # Errors
    ///
    /// Returns an error from flush or `sync_all`.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()
    }

    /// Current destination path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate_if_needed(&mut self, incoming: u64) -> io::Result<()> {
        if self.bytes_written == 0 {
            return Ok(());
        }
        let size_due = self
            .policy
            .max_bytes
            .is_some_and(|limit| self.bytes_written.saturating_add(incoming) > limit);
        let age_due = self.policy.max_age.is_some_and(|limit| {
            SystemTime::now()
                .duration_since(self.opened_at)
                .is_ok_and(|age| age >= limit)
        });
        if size_due || age_due {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.sync()?;
        let archive = self.next_archive_path()?;
        fs::rename(&self.path, &archive)?;
        let replacement = match open_append(&self.path) {
            Ok(file) => file,
            Err(error) => {
                let _rollback = fs::rename(&archive, &self.path);
                return Err(error);
            }
        };
        self.file = replacement;
        self.bytes_written = 0;
        self.opened_at = SystemTime::now();
        sync_parent(&self.path)?;
        self.enforce_retention()
    }

    fn next_archive_path(&mut self) -> io::Result<PathBuf> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let filename = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "log filename is not UTF-8")
            })?;
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        for _ in 0..1024 {
            self.archive_sequence = self.archive_sequence.saturating_add(1);
            let candidate = parent.join(format!(
                "{filename}.immortal-archive.{timestamp}.{}.{}",
                std::process::id(),
                self.archive_sequence
            ));
            if !candidate.exists() {
                return Ok(candidate);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to allocate unique log archive name",
        ))
    }

    fn enforce_retention(&mut self) -> io::Result<()> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        let filename = self
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "log filename is not UTF-8")
            })?;
        let prefix = format!("{filename}.immortal-archive.");
        let mut archives = Vec::new();
        for entry in fs::read_dir(parent)? {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Some(identity) = archive_identity(name, &prefix) else {
                continue;
            };
            let metadata = fs::symlink_metadata(&path)?;
            if metadata.is_file() && !metadata.file_type().is_symlink() {
                archives.push((identity, path, metadata.len()));
            }
        }
        archives.sort_by_key(|(identity, _, _)| *identity);
        let mut total_bytes = archives
            .iter()
            .fold(0_u64, |total, (_, _, length)| total.saturating_add(*length));
        let mut remaining = archives.len();
        for (_, archive, length) in archives {
            let over_count = self.policy.keep.is_some_and(|keep| remaining > keep);
            let over_bytes = self
                .policy
                .max_total_bytes
                .is_some_and(|limit| total_bytes > limit);
            if !(over_count || over_bytes) {
                break;
            }
            fs::remove_file(archive)?;
            remaining = remaining.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(length);
        }
        Ok(())
    }
}

fn archive_identity(name: &str, prefix: &str) -> Option<(u128, u32, u64)> {
    let mut fields = name.strip_prefix(prefix)?.split('.');
    let timestamp = fields.next()?.parse().ok()?;
    let process = fields.next()?.parse().ok()?;
    let sequence = fields.next()?.parse().ok()?;
    fields
        .next()
        .is_none()
        .then_some((timestamp, process, sequence))
}

fn validate_rotation_policy(policy: RotationPolicy) -> io::Result<()> {
    if policy.max_bytes == Some(0)
        || policy.max_age == Some(Duration::ZERO)
        || policy.keep == Some(0)
        || policy.max_total_bytes == Some(0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configured rotation limits must be greater than zero",
        ));
    }
    Ok(())
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        BackpressurePolicy, LoggingPlan, LoggingRuntime, LoggingShutdown, LoggingShutdownEffect,
        OutputStream, PipelineHealth, RotatingFile, RotationPolicy,
    };
    use crate::config::parse_str;
    use crate::supervisor::{FailureReason, Generation, SupervisorState};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortal-logging-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn file_and_logger_normalize_to_combined_route_and_shared_sink() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/out.log\nlogger: [/usr/bin/logger, -t, service]\n",
        )?;
        let plan = LoggingPlan::from_config(&config.logging)?;
        assert_eq!(plan.local_files.len(), 1);
        let route = plan.local_files.first().ok_or("file route missing")?;
        assert_eq!(route.stream, OutputStream::Combined);
        assert_eq!(route.backpressure, BackpressurePolicy::LosslessBlock);
        assert_eq!(
            plan.logger,
            Some(vec![
                "/usr/bin/logger".to_owned(),
                "-t".to_owned(),
                "service".to_owned()
            ])
        );
        Ok(())
    }

    #[test]
    fn explicit_streams_create_independent_local_routes() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  stdout:\n    file: /tmp/out.log\n  stderr:\n    file: /tmp/err.log\n",
        )?;
        let plan = LoggingPlan::from_config(&config.logging)?;
        assert_eq!(plan.local_files.len(), 2);
        assert_eq!(
            plan.local_files.first().map(|value| value.stream),
            Some(OutputStream::Stdout)
        );
        assert_eq!(
            plan.local_files.get(1).map(|value| value.stream),
            Some(OutputStream::Stderr)
        );
        assert_eq!(plan.logger, None);
        Ok(())
    }

    #[test]
    fn combined_route_covers_both_child_streams() -> Result<(), Box<dyn Error>> {
        let config = parse_str("version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/out.log\n")?;
        let mut runtime = LoggingRuntime::new(LoggingPlan::from_config(&config.logging)?);
        let combined = runtime
            .pipe(OutputStream::Combined)
            .ok_or("combined pipe missing")?;
        assert_eq!(runtime.pipe(OutputStream::Stdout), Some(combined));
        assert_eq!(runtime.pipe(OutputStream::Stderr), Some(combined));
        assert_eq!(runtime.stages(OutputStream::Stdout).len(), 1);
        assert_eq!(runtime.stages(OutputStream::Stderr).len(), 1);

        runtime.update_stage(
            OutputStream::Stdout,
            0,
            1,
            SupervisorState::Ready(Generation::FIRST),
        )?;
        assert_eq!(
            runtime.health(OutputStream::Stdout),
            Some(PipelineHealth::Ready)
        );
        assert_eq!(
            runtime.health(OutputStream::Stderr),
            Some(PipelineHealth::Ready)
        );
        Ok(())
    }

    #[test]
    fn logger_restart_preserves_pipe_and_reports_health() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/out.log\nlogger: [/usr/bin/logger, -t, service]\n",
        )?;
        let mut runtime = LoggingRuntime::new(LoggingPlan::from_config(&config.logging)?);
        let pipe = runtime.pipe(OutputStream::Combined).ok_or("pipe missing")?;
        let logger_pipe = runtime.logger_pipe().ok_or("logger pipe missing")?;
        assert_eq!(
            runtime.health(OutputStream::Combined),
            Some(PipelineHealth::Starting)
        );

        runtime.update_stage(
            OutputStream::Combined,
            0,
            1,
            SupervisorState::Ready(Generation::FIRST),
        )?;
        runtime.update_logger(1, SupervisorState::Ready(Generation::FIRST))?;
        assert_eq!(
            runtime.health(OutputStream::Combined),
            Some(PipelineHealth::Ready)
        );
        runtime.update_logger(
            2,
            SupervisorState::Backoff {
                generation: Generation::FIRST,
                delay_seconds: 2,
            },
        )?;
        assert_eq!(runtime.pipe(OutputStream::Combined), Some(pipe));
        assert_eq!(runtime.logger_pipe(), Some(logger_pipe));
        assert_eq!(
            runtime.health(OutputStream::Combined),
            Some(PipelineHealth::Backoff)
        );
        runtime.update_logger(2, SupervisorState::Failed(FailureReason::RetryLimit))?;
        assert_eq!(
            runtime.health(OutputStream::Combined),
            Some(PipelineHealth::Failed)
        );
        Ok(())
    }

    #[test]
    fn shutdown_order_cannot_stop_loggers_before_service_and_drain() -> Result<(), Box<dyn Error>> {
        let mut shutdown = LoggingShutdown::default();
        assert!(shutdown.drain_finished().is_err());
        assert_eq!(shutdown.begin(true)?, LoggingShutdownEffect::StopService);
        assert!(shutdown.loggers_stopped().is_err());
        assert_eq!(
            shutdown.service_stopped()?,
            LoggingShutdownEffect::BeginDrain
        );
        assert_eq!(
            shutdown.drain_finished()?,
            LoggingShutdownEffect::StopLoggersDownstreamFirst
        );
        assert_eq!(shutdown.loggers_stopped()?, LoggingShutdownEffect::Complete);
        Ok(())
    }

    #[test]
    fn rotation_syncs_and_enforces_archive_count() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.log");
        let mut sink = RotatingFile::open(
            &path,
            RotationPolicy {
                max_bytes: Some(5),
                keep: Some(2),
                ..RotationPolicy::default()
            },
        )?;
        for bytes in [b"aaa".as_slice(), b"bbb", b"ccc", b"ddd"] {
            sink.write_all(bytes)?;
        }
        sink.sync()?;

        assert_eq!(fs::read(&path)?, b"ddd");
        let archives = archive_contents(directory.path(), "api.log")?;
        assert_eq!(archives, [b"bbb".to_vec(), b"ccc".to_vec()]);
        Ok(())
    }

    #[test]
    fn rotation_enforces_total_archive_bytes() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.log");
        let mut sink = RotatingFile::open(
            &path,
            RotationPolicy {
                max_bytes: Some(3),
                keep: Some(10),
                max_total_bytes: Some(3),
                ..RotationPolicy::default()
            },
        )?;
        sink.write_all(b"aaa")?;
        sink.write_all(b"bbb")?;
        sink.write_all(b"ccc")?;

        let archives = archive_contents(directory.path(), "api.log")?;
        assert_eq!(archives, [b"bbb".to_vec()]);
        Ok(())
    }

    #[test]
    fn reopen_recovers_rotation_state_without_removing_unowned_files() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.log");
        let older = directory.path().join("api.log.immortal-archive.100.7.1");
        let newer = directory.path().join("api.log.immortal-archive.200.7.2");
        let unrelated = directory
            .path()
            .join("api.log.immortal-archive.operator-copy");
        fs::write(&older, b"old")?;
        fs::write(&newer, b"new")?;
        fs::write(&unrelated, b"operator")?;

        let mut sink = RotatingFile::open(
            &path,
            RotationPolicy {
                keep: Some(1),
                ..RotationPolicy::default()
            },
        )?;
        sink.write_all(b"live")?;
        sink.sync()?;

        assert!(!older.exists());
        assert_eq!(fs::read(newer)?, b"new");
        assert_eq!(fs::read(unrelated)?, b"operator");
        assert_eq!(fs::read(path)?, b"live");
        Ok(())
    }

    #[test]
    fn sink_rejects_invalid_limits_and_missing_parent() {
        assert!(
            RotatingFile::open(
                Path::new("/definitely/missing/immortal/api.log"),
                RotationPolicy::default(),
            )
            .is_err()
        );
        assert!(
            RotatingFile::open(
                Path::new("api.log"),
                RotationPolicy {
                    max_bytes: Some(0),
                    ..RotationPolicy::default()
                },
            )
            .is_err()
        );
    }

    fn archive_contents(directory: &Path, filename: &str) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
        let prefix = format!("{filename}.immortal-archive.");
        let mut paths = Vec::new();
        for entry in fs::read_dir(directory)? {
            let path = entry?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with(&prefix))
            {
                paths.push(path);
            }
        }
        paths.sort();
        paths
            .into_iter()
            .map(|path| fs::read(path).map_err(Into::into))
            .collect()
    }
}
