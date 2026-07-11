//! Supervised external-logger pipeline plans and shutdown ordering.
//!
//! This module deliberately models ownership and lifecycle, not byte copying.
//! A service writes to one stable pipe; each configured stage reads from the
//! previous stage. Lossless kernel-pipe backpressure is the only default.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    config::{FileLogConfig, LoggingConfig, OutputConfig},
    supervisor::SupervisorState,
};

/// Child output stream attached to one pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    /// Standard output only.
    Stdout,
    /// Standard error only.
    Stderr,
    /// Standard output and error share one pipe for Go compatibility.
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

/// One service-output pipeline, ordered from service to final consumer.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PipelinePlan {
    /// Service descriptors connected to this chain.
    pub stream: OutputStream,
    /// File adapter followed by an optional external command.
    pub stages: Vec<LoggerStage>,
    /// Loss policy between processes.
    pub backpressure: BackpressurePolicy,
}

/// Complete logger plan for one service.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoggingPlan {
    /// Zero, one, or two independent pipelines.
    pub pipelines: Vec<PipelinePlan>,
}

impl LoggingPlan {
    /// Normalize output configuration into process chains.
    ///
    /// A file-plus-command route becomes `service -> immortallog -> external`
    /// rather than an in-supervisor multiwriter.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty external command or conflicting combined
    /// and explicit stderr routes.
    pub fn from_config(config: &LoggingConfig) -> Result<Self, LoggingError> {
        let stdout = pipeline_stages(&config.stdout)?;
        let stderr = pipeline_stages(&config.stderr)?;
        if config.combine_stderr && !stderr.is_empty() {
            return Err(LoggingError::CombinedWithStderrRoute);
        }

        let mut pipelines = Vec::new();
        if !stdout.is_empty() {
            pipelines.push(PipelinePlan {
                stream: if config.combine_stderr {
                    OutputStream::Combined
                } else {
                    OutputStream::Stdout
                },
                stages: stdout,
                backpressure: BackpressurePolicy::LosslessBlock,
            });
        }
        if !stderr.is_empty() {
            pipelines.push(PipelinePlan {
                stream: OutputStream::Stderr,
                stages: stderr,
                backpressure: BackpressurePolicy::LosslessBlock,
            });
        }
        Ok(Self { pipelines })
    }
}

fn pipeline_stages(output: &OutputConfig) -> Result<Vec<LoggerStage>, LoggingError> {
    let mut stages = Vec::new();
    if output.file.file.is_some() {
        stages.push(LoggerStage::FileAdapter(output.file.clone()));
    }
    if let Some(command) = &output.logger {
        if command.first().is_none_or(String::is_empty) {
            return Err(LoggingError::EmptyExternalCommand);
        }
        stages.push(LoggerStage::External(command.clone()));
    }
    Ok(stages)
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
    pipelines: Vec<PipelineRuntime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PipelineRuntime {
    plan: PipelinePlan,
    pipe: PipeId,
    stages: Vec<LoggerStageStatus>,
}

impl LoggingRuntime {
    /// Allocate durable pipe identities before starting any logger or service.
    #[must_use]
    pub fn new(plan: LoggingPlan) -> Self {
        let pipelines = plan
            .pipelines
            .into_iter()
            .enumerate()
            .map(|(position, plan)| {
                let stages = plan
                    .stages
                    .iter()
                    .cloned()
                    .map(|stage| LoggerStageStatus {
                        stage,
                        generation: 0,
                        state: SupervisorState::Down,
                    })
                    .collect();
                PipelineRuntime {
                    plan,
                    pipe: PipeId(
                        u64::try_from(position)
                            .unwrap_or(u64::MAX)
                            .saturating_add(1),
                    ),
                    stages,
                }
            })
            .collect();
        Self { pipelines }
    }

    /// Stable pipe identity for an output stream.
    #[must_use]
    pub fn pipe(&self, stream: OutputStream) -> Option<PipeId> {
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.plan.stream == stream)
            .map(|pipeline| pipeline.pipe)
    }

    /// Replace one logger generation/state without replacing its pipe.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown stream or stage index.
    pub fn update_stage(
        &mut self,
        stream: OutputStream,
        stage_index: usize,
        generation: u64,
        lifecycle_state: SupervisorState,
    ) -> Result<(), LoggingError> {
        let pipeline = self
            .pipelines
            .iter_mut()
            .find(|pipeline| pipeline.plan.stream == stream)
            .ok_or(LoggingError::UnknownPipeline)?;
        let stage_status = pipeline
            .stages
            .get_mut(stage_index)
            .ok_or(LoggingError::UnknownStage)?;
        stage_status.generation = generation;
        stage_status.state = lifecycle_state;
        Ok(())
    }

    /// Aggregate health for one pipeline.
    #[must_use]
    pub fn health(&self, stream: OutputStream) -> Option<PipelineHealth> {
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.plan.stream == stream)
            .map(|pipeline| summarize_health(&pipeline.stages))
    }

    /// Immutable stage statuses for status publication.
    #[must_use]
    pub fn stages(&self, stream: OutputStream) -> Option<&[LoggerStageStatus]> {
        self.pipelines
            .iter()
            .find(|pipeline| pipeline.plan.stream == stream)
            .map(|pipeline| pipeline.stages.as_slice())
    }
}

fn summarize_health(stages: &[LoggerStageStatus]) -> PipelineHealth {
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
    /// Combined stderr conflicts with an explicit stderr chain.
    CombinedWithStderrRoute,
    /// Runtime stream does not have a pipeline.
    UnknownPipeline,
    /// Pipeline stage index does not exist.
    UnknownStage,
    /// Shutdown event arrived out of order.
    InvalidShutdownTransition,
    /// Archive count cannot fit the target platform.
    RetentionCountTooLarge,
}

impl Display for LoggingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExternalCommand => formatter.write_str("external logger command is empty"),
            Self::CombinedWithStderrRoute => {
                formatter.write_str("combined stderr conflicts with explicit stderr logging")
            }
            Self::UnknownPipeline => formatter.write_str("unknown logging pipeline"),
            Self::UnknownStage => formatter.write_str("unknown logger stage"),
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
            max_total_bytes: config.max_total_bytes,
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
        BackpressurePolicy, LoggerStage, LoggingPlan, LoggingRuntime, LoggingShutdown,
        LoggingShutdownEffect, OutputStream, PipelineHealth, RotatingFile, RotationPolicy,
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
    fn file_and_logger_become_one_combined_process_chain() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlogging:\n  combine_stderr: true\n  stdout:\n    file:\n      file: /tmp/out.log\n    logger: [/usr/bin/logger, -t, service]\n",
        )?;
        let plan = LoggingPlan::from_config(&config.logging)?;
        assert_eq!(plan.pipelines.len(), 1);
        let pipeline = plan.pipelines.first().ok_or("pipeline missing")?;
        assert_eq!(pipeline.stream, OutputStream::Combined);
        assert_eq!(pipeline.backpressure, BackpressurePolicy::LosslessBlock);
        assert!(matches!(
            pipeline.stages.first(),
            Some(LoggerStage::FileAdapter(_))
        ));
        assert!(matches!(
            pipeline.stages.get(1),
            Some(LoggerStage::External(_))
        ));
        Ok(())
    }

    #[test]
    fn explicit_stderr_creates_independent_pipeline() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlogging:\n  stdout:\n    file:\n      file: /tmp/out.log\n  stderr:\n    file:\n      file: /tmp/err.log\n",
        )?;
        let plan = LoggingPlan::from_config(&config.logging)?;
        assert_eq!(plan.pipelines.len(), 2);
        assert_eq!(
            plan.pipelines.first().map(|value| value.stream),
            Some(OutputStream::Stdout)
        );
        assert_eq!(
            plan.pipelines.get(1).map(|value| value.stream),
            Some(OutputStream::Stderr)
        );
        Ok(())
    }

    #[test]
    fn logger_restart_preserves_pipe_and_reports_health() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [/bin/true]\nlogging:\n  combine_stderr: true\n  stdout:\n    file:\n      file: /tmp/out.log\n    logger: [/usr/bin/logger, -t, service]\n",
        )?;
        let mut runtime = LoggingRuntime::new(LoggingPlan::from_config(&config.logging)?);
        let pipe = runtime.pipe(OutputStream::Combined).ok_or("pipe missing")?;
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
        runtime.update_stage(
            OutputStream::Combined,
            1,
            1,
            SupervisorState::Ready(Generation::FIRST),
        )?;
        assert_eq!(
            runtime.health(OutputStream::Combined),
            Some(PipelineHealth::Ready)
        );
        runtime.update_stage(
            OutputStream::Combined,
            1,
            2,
            SupervisorState::Backoff {
                generation: Generation::FIRST,
                delay_seconds: 2,
            },
        )?;
        assert_eq!(runtime.pipe(OutputStream::Combined), Some(pipe));
        assert_eq!(
            runtime.health(OutputStream::Combined),
            Some(PipelineHealth::Backoff)
        );
        runtime.update_stage(
            OutputStream::Combined,
            1,
            2,
            SupervisorState::Failed(FailureReason::RetryLimit),
        )?;
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
