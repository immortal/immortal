//! Bounded discovery and desired-state reconciliation for `immortaldir`.
//!
//! A scan reads stable, size-limited definition snapshots and isolates invalid
//! candidates. The desired-state tracker converts complete scans into semantic
//! actions with confirmed deletion, while [`DefinitionSnapshots`] atomically
//! publishes normalized launch/applied state and a bounded deletion ledger
//! below an owner-only runtime root. Applied snapshots remain configuration
//! authority; the ledger holds only names and absence counts, and retains a
//! confirmed deletion until its supervisor and applied state are gone.
//! [`SupervisorLauncher`] moves one pre-Tokio broker client through bounded
//! checked daemon-launch batches; it owns mechanism only, leaving lifecycle
//! policy to the calling reconciliation loop.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter, Write as FmtWrite},
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    num::NonZeroUsize,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, SystemTime},
};

use tokio::time::timeout;

use crate::{
    config::{ConfigError, MAX_CONFIG_BYTES, ServiceConfig, emit_config, parse_bytes_at},
    platform::file_identity,
    process::{
        BrokerTaskId, ChildEvent, ProcessBrokerEndpoint, ProcessBrokerEvent, ProcessCommand,
        wait_for_event,
    },
};

/// Default maximum number of candidate definitions accepted in one directory.
pub const DEFAULT_MAX_DEFINITIONS: usize = 4096;
/// Consecutive authoritative scans required before a missing definition is removed.
pub const DEFAULT_DELETION_CONFIRMATIONS: usize = 2;
/// Default maximum checked supervisor launches submitted at once.
pub const DEFAULT_MAX_CONCURRENT_LAUNCHES: usize = 8;
/// Hard upper bound for one checked supervisor-launch batch.
pub const MAX_CONCURRENT_LAUNCHES: usize = 64;
const SNAPSHOT_DIRECTORY: &str = ".definitions";
const TRACKER_STATE_FILE: &str = "tracker.state";
const TRACKER_STATE_VERSION: usize = 1;
const MAX_TRACKER_STATE_BYTES: usize = 2 * 1024 * 1024;
const SUPERVISOR_START_TIMEOUT: Duration = Duration::from_secs(15);
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

/// Validated upper bound for one concurrent supervisor-launch batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaunchConcurrency(NonZeroUsize);

impl LaunchConcurrency {
    /// Validate a nonzero launch limit within [`MAX_CONCURRENT_LAUNCHES`].
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is zero or exceeds the hard resource bound.
    pub const fn new(value: usize) -> Result<Self, LaunchConcurrencyError> {
        match NonZeroUsize::new(value) {
            Some(value) if value.get() <= MAX_CONCURRENT_LAUNCHES => Ok(Self(value)),
            Some(value) => Err(LaunchConcurrencyError::TooLarge(value)),
            None => Err(LaunchConcurrencyError::Zero),
        }
    }

    /// Return the validated batch limit.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for LaunchConcurrency {
    fn default() -> Self {
        Self(NonZeroUsize::new(DEFAULT_MAX_CONCURRENT_LAUNCHES).unwrap_or(NonZeroUsize::MIN))
    }
}

/// Invalid concurrent supervisor-launch limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchConcurrencyError {
    /// A concurrency limit must permit at least one launch.
    Zero,
    /// The requested limit exceeds the hard resource bound.
    TooLarge(NonZeroUsize),
}

impl Display for LaunchConcurrencyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => formatter.write_str("launch concurrency must be nonzero"),
            Self::TooLarge(value) => write!(
                formatter,
                "launch concurrency {} exceeds maximum {MAX_CONCURRENT_LAUNCHES}",
                value.get()
            ),
        }
    }
}

impl Error for LaunchConcurrencyError {}

/// Resource limits for one authoritative directory scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanLimits {
    /// Maximum number of top-level `*.yml` candidates.
    pub max_definitions: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_definitions: DEFAULT_MAX_DEFINITIONS,
        }
    }
}

/// One valid desired service definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Definition {
    /// Safe filename stem used as service identity.
    pub name: String,
    /// Source file observed during this scan.
    pub path: PathBuf,
    /// Parsed and normalized service configuration.
    pub config: ServiceConfig,
}

/// Non-fatal problem isolated to one scan or candidate.
#[derive(Debug)]
pub struct ScanProblem {
    /// Candidate involved, or the directory for a global limit.
    pub path: PathBuf,
    /// Stable problem category.
    pub kind: ScanProblemKind,
}

/// Stable directory-scan problem category.
#[derive(Debug)]
pub enum ScanProblemKind {
    /// Candidate count exceeded [`ScanLimits::max_definitions`].
    DefinitionLimit,
    /// Candidate service name is unsafe.
    UnsafeName,
    /// More than one candidate resolved to the same service identity.
    DuplicateName,
    /// Candidate is a symbolic link.
    Symlink,
    /// Candidate is not a regular file.
    NotRegular,
    /// Candidate could not be opened, read, or inspected.
    Io(io::Error),
    /// Candidate changed while its snapshot was being read.
    ChangedDuringRead,
    /// Candidate configuration is oversized, malformed, or invalid.
    Config(ConfigError),
}

/// Result of one complete authoritative directory scan.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Valid definitions keyed by safe service name.
    pub definitions: BTreeMap<String, Definition>,
    /// Isolated problems which do not invalidate other services.
    pub problems: Vec<ScanProblem>,
}

/// Failure to inspect the definitions directory itself.
#[derive(Debug)]
pub struct ScanError(io::Error);

impl Display for ScanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unable to scan definitions directory: {}",
            self.0
        )
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Secure store for immutable normalized definitions passed to new supervisors.
#[derive(Debug)]
pub struct DefinitionSnapshots {
    directory: PathBuf,
    owner_uid: u32,
}

impl DefinitionSnapshots {
    /// Open or create the owner-only hidden snapshot directory below a runtime root.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime root is unsafe or the snapshot directory
    /// is a symlink, has a different owner, or is not mode `0700`.
    pub fn open(runtime_root: &Path) -> io::Result<Self> {
        crate::runtime::discover(runtime_root).map_err(io::Error::other)?;
        let root = fs::symlink_metadata(runtime_root)?;
        let directory = runtime_root.join(SNAPSHOT_DIRECTORY);
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != root.uid()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe reconciliation snapshot directory",
            ));
        }
        Ok(Self {
            directory,
            owner_uid: metadata.uid(),
        })
    }

    /// Atomically publish one normalized definition and return its stable path.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name, serialization, exclusive creation,
    /// write, sync, rename, or parent-directory sync failure.
    pub fn publish(&self, name: &str, config: &ServiceConfig) -> io::Result<PathBuf> {
        self.publish_named(name, "launch", config)
    }

    /// Persist the last desired state successfully applied by reconciliation.
    ///
    /// # Errors
    ///
    /// Returns the same validation, serialization, and atomic-write failures as
    /// [`Self::publish`].
    pub fn record_applied(&self, name: &str, config: &ServiceConfig) -> io::Result<()> {
        let _path = self.publish_named(name, "applied", config)?;
        Ok(())
    }

    /// Load the last applied desired state without following replacement links.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name, unsafe entry type/ownership/mode, or
    /// configuration parse and validation failure.
    pub fn load_applied(&self, name: &str) -> io::Result<Option<ServiceConfig>> {
        let path = self.named_path(name, "applied")?;
        let Some(bytes) = self.read_owned_file(&path, MAX_CONFIG_BYTES)? else {
            return Ok(None);
        };
        parse_bytes_at(&bytes, &path)
            .map(Some)
            .map_err(io::Error::other)
    }

    /// Remove applied state after a definition is stably deleted and halted.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name or filesystem removal failure.
    pub fn remove_applied(&self, name: &str) -> io::Result<()> {
        let path = self.named_path(name, "applied")?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != self.owner_uid
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe applied-state snapshot",
            ));
        }
        fs::remove_file(path)
    }

    /// Restore desired configurations and deletion confirmations after restart.
    ///
    /// Applied snapshots remain the configuration authority. The bounded tracker
    /// ledger contributes only safe service names and consecutive-absence counts;
    /// stale ledger entries without an applied snapshot are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe snapshot entries, an oversized or malformed
    /// ledger, duplicate names, unsupported versions, or invalid applied state.
    pub fn load_tracker(&self) -> io::Result<DesiredStateTracker> {
        let mut counts = self.load_tracker_counts()?;
        for name in self.applied_names()? {
            counts.entry(name).or_insert(0);
        }

        let deletion_confirmations =
            NonZeroUsize::new(DEFAULT_DELETION_CONFIRMATIONS).unwrap_or(NonZeroUsize::MIN);
        let mut tracker = DesiredStateTracker::new(deletion_confirmations);
        for (name, count) in counts {
            if count > deletion_confirmations.get() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker absence count exceeds deletion threshold",
                ));
            }
            let Some(config) = self.load_applied(&name)? else {
                continue;
            };
            tracker.desired.insert(name.clone(), config);
            if count > 0 {
                tracker.absent_scans.insert(name, count);
            }
        }
        Ok(tracker)
    }

    /// Atomically checkpoint desired names and deletion confirmations.
    ///
    /// The ledger deliberately excludes configuration contents, which remain in
    /// validated applied snapshots, and retains confirmed deletions until the
    /// caller acknowledges successful supervisor cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error for serialization bounds or atomic write/sync failures.
    pub fn record_tracker(&self, tracker: &DesiredStateTracker) -> io::Result<()> {
        let mut names: BTreeSet<&str> = tracker.desired.keys().map(String::as_str).collect();
        names.extend(tracker.absent_scans.keys().map(String::as_str));
        if names.len() > DEFAULT_MAX_DEFINITIONS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tracker service count exceeds definition limit",
            ));
        }

        let mut contents = format!("version\t{TRACKER_STATE_VERSION}\n");
        for name in names {
            let count = tracker.absent_scans.get(name).copied().unwrap_or(0);
            if count > tracker.deletion_confirmations.get() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker absence count exceeds deletion threshold",
                ));
            }
            writeln!(&mut contents, "{name}\t{count}").map_err(io::Error::other)?;
            if contents.len() > MAX_TRACKER_STATE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker state exceeds size limit",
                ));
            }
        }
        let destination = self.directory.join(TRACKER_STATE_FILE);
        self.atomic_replace(&destination, "tracker", contents.as_bytes())
    }

    fn load_tracker_counts(&self) -> io::Result<BTreeMap<String, usize>> {
        let path = self.directory.join(TRACKER_STATE_FILE);
        let Some(bytes) = self.read_owned_file(&path, MAX_TRACKER_STATE_BYTES)? else {
            return Ok(BTreeMap::new());
        };
        let contents = std::str::from_utf8(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tracker state is not UTF-8: {error}"),
            )
        })?;
        let mut lines = contents.lines();
        let header = format!("version\t{TRACKER_STATE_VERSION}");
        if lines.next() != Some(header.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported tracker state version",
            ));
        }

        let mut counts = BTreeMap::new();
        for line in lines {
            let Some((name, count)) = line.split_once('\t') else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed tracker state entry",
                ));
            };
            if !safe_definition_name(name) || count.contains('\t') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe tracker state entry",
                ));
            }
            let count = count.parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid tracker absence count: {error}"),
                )
            })?;
            if counts.insert(name.to_owned(), count).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate tracker state entry",
                ));
            }
            if counts.len() > DEFAULT_MAX_DEFINITIONS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker service count exceeds definition limit",
                ));
            }
        }
        Ok(counts)
    }

    fn applied_names(&self) -> io::Result<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(name) = file_name.strip_suffix(".applied.yml") else {
                continue;
            };
            if !safe_definition_name(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe applied-state snapshot name",
                ));
            }
            names.insert(name.to_owned());
            if names.len() > DEFAULT_MAX_DEFINITIONS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "applied-state snapshot count exceeds definition limit",
                ));
            }
        }
        Ok(names)
    }

    fn read_owned_file(&self, path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !self.state_file_is_safe(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe reconciliation state file",
            ));
        }
        if metadata.len() > limit as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state file exceeds size limit",
            ));
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let before = file.metadata()?;
        if !self.state_file_is_safe(&before) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe opened reconciliation state file",
            ));
        }
        if metadata_changed(&metadata, &before) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state changed before open",
            ));
        }
        if before.len() > limit as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state file exceeds size limit",
            ));
        }
        let capacity = usize::try_from(before.len()).map_or(limit, |size| size.min(limit));
        let mut bytes = Vec::with_capacity(capacity);
        (&mut file)
            .take(limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        let file_after = file.metadata()?;
        let path_after = fs::symlink_metadata(path)?;
        if bytes.len() > limit
            || !self.state_file_is_safe(&file_after)
            || !self.state_file_is_safe(&path_after)
            || metadata_changed(&before, &file_after)
            || metadata_changed(&before, &path_after)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state changed during read",
            ));
        }
        Ok(Some(bytes))
    }

    fn state_file_is_safe(&self, metadata: &Metadata) -> bool {
        metadata.is_file() && metadata.uid() == self.owner_uid && metadata.mode() & 0o777 == 0o600
    }

    fn publish_named(&self, name: &str, kind: &str, config: &ServiceConfig) -> io::Result<PathBuf> {
        let contents = emit_config(config).map_err(io::Error::other)?;
        let destination = self.named_path(name, kind)?;
        self.atomic_replace(&destination, name, contents.as_bytes())?;
        Ok(destination)
    }

    fn atomic_replace(&self, destination: &Path, label: &str, contents: &[u8]) -> io::Result<()> {
        let sequence = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let temporary =
            self.directory
                .join(format!(".{label}.{}.{}.tmp", std::process::id(), sequence));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let result = (|| -> io::Result<()> {
            let mut file = options.open(&temporary)?;
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&temporary, destination)?;
            File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ignored = fs::remove_file(&temporary);
        }
        result
    }

    fn named_path(&self, name: &str, kind: &str) -> io::Result<PathBuf> {
        if !safe_definition_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe snapshot service name",
            ));
        }
        Ok(self.directory.join(format!("{name}.{kind}.yml")))
    }
}

fn safe_definition_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

/// Failure while launching a checked supervisor through the pre-Tokio broker.
#[derive(Debug)]
pub enum LauncherError {
    /// Launcher command configuration was invalid.
    Config(ConfigError),
    /// Broker transport or process preparation failed.
    Broker(crate::process::ProcessBrokerError),
    /// Supervisor launcher could not be prepared or the broker could not be reaped.
    OperatingSystem(io::Error),
    /// Launcher did not complete within its hard deadline.
    Timeout,
    /// A caller submitted more launches than its validated batch limit.
    BatchLimit { actual: usize, limit: usize },
    /// Broker returned an event unrelated to the current launch task.
    UnexpectedEvent(ProcessBrokerEvent),
    /// The checked `immortal` launcher exited unsuccessfully.
    LauncherExited(ChildEvent),
}

impl Display for LauncherError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => Display::fmt(error, formatter),
            Self::Broker(error) => Display::fmt(error, formatter),
            Self::OperatingSystem(error) => Display::fmt(error, formatter),
            Self::Timeout => formatter.write_str("supervisor launcher deadline exceeded"),
            Self::BatchLimit { actual, limit } => {
                write!(
                    formatter,
                    "launch batch size {actual} exceeds limit {limit}"
                )
            }
            Self::UnexpectedEvent(event) => {
                write!(formatter, "unexpected supervisor launcher event: {event:?}")
            }
            Self::LauncherExited(event) => {
                write!(
                    formatter,
                    "supervisor launcher exited unsuccessfully: {event:?}"
                )
            }
        }
    }
}

impl Error for LauncherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Broker(error) => Some(error),
            Self::OperatingSystem(error) => Some(error),
            Self::Timeout
            | Self::BatchLimit { .. }
            | Self::UnexpectedEvent(_)
            | Self::LauncherExited(_) => None,
        }
    }
}

impl From<ConfigError> for LauncherError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<crate::process::ProcessBrokerError> for LauncherError {
    fn from(error: crate::process::ProcessBrokerError) -> Self {
        Self::Broker(error)
    }
}

impl From<io::Error> for LauncherError {
    fn from(error: io::Error) -> Self {
        Self::OperatingSystem(error)
    }
}

/// Checked-daemon launcher backed by one broker created before Tokio.
pub struct SupervisorLauncher {
    client: crate::process::ProcessBrokerClient,
    next_task: u64,
}

/// Owned paths for one checked supervisor launch.
#[derive(Debug, Eq, PartialEq)]
pub struct SupervisorLaunch {
    snapshot: PathBuf,
    runtime_directory: PathBuf,
}

impl SupervisorLaunch {
    /// Construct one launch from a normalized snapshot and exact runtime path.
    #[must_use]
    pub fn new(snapshot: PathBuf, runtime_directory: PathBuf) -> Self {
        Self {
            snapshot,
            runtime_directory,
        }
    }
}

/// Isolated result for one submitted launcher task.
#[derive(Debug)]
pub enum LauncherTaskError {
    /// The broker could not execute the launcher command.
    Spawn,
    /// The launcher invoked `immortal` but it exited unsuccessfully.
    Exited(ChildEvent),
}

impl Display for LauncherTaskError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn => formatter.write_str("broker could not execute the immortal launcher"),
            Self::Exited(event) => write!(formatter, "supervisor launcher failed: {event:?}"),
        }
    }
}

impl Error for LauncherTaskError {}

impl From<LauncherTaskError> for LauncherError {
    fn from(error: LauncherTaskError) -> Self {
        match error {
            LauncherTaskError::Spawn => {
                Self::OperatingSystem(io::Error::other("broker could not execute launcher"))
            }
            LauncherTaskError::Exited(event) => Self::LauncherExited(event),
        }
    }
}

#[derive(Debug, Default)]
struct LaunchTaskState {
    started: bool,
    outcome: Option<Result<(), LauncherTaskError>>,
}

impl SupervisorLauncher {
    /// Connect an endpoint and require the broker's bounded readiness event.
    ///
    /// # Errors
    ///
    /// Returns a broker, timeout, or unexpected-event failure.
    pub async fn connect(endpoint: ProcessBrokerEndpoint) -> Result<Self, LauncherError> {
        let mut client = endpoint.connect()?;
        match timeout(SUPERVISOR_START_TIMEOUT, client.next_event()).await {
            Ok(Ok(ProcessBrokerEvent::Ready)) => Ok(Self {
                client,
                next_task: 1,
            }),
            Ok(Ok(event)) => Err(LauncherError::UnexpectedEvent(event)),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(LauncherError::Timeout),
        }
    }

    /// Launch one checked daemon and wait for its invoking process to report success.
    ///
    /// # Errors
    ///
    /// Returns preparation, broker, timeout, exec, or nonzero launcher-exit failure.
    pub async fn launch(
        &mut self,
        binary: &Path,
        snapshot: &Path,
        runtime_directory: &Path,
    ) -> Result<(), LauncherError> {
        let command = prepare_launcher_command(binary, snapshot, runtime_directory)?;
        let task = self.allocate_task()?;
        self.client
            .spawn_task(task, command, SUPERVISOR_START_TIMEOUT)
            .await?;
        timeout(SUPERVISOR_START_TIMEOUT, self.wait_for_launcher(task))
            .await
            .map_err(|_| LauncherError::Timeout)?
    }

    /// Submit one bounded batch and return task outcomes in input order.
    ///
    /// Commands and task IDs are fully prepared before the first broker write.
    /// A task-local spawn/nonzero-exit failure does not discard other outcomes;
    /// broker, protocol, or deadline failure aborts the complete batch.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized batch, invalid command preparation,
    /// task-space exhaustion, broker transport/protocol failure, or the shared
    /// batch deadline. An empty batch succeeds with an empty result.
    pub async fn launch_batch(
        &mut self,
        binary: &Path,
        launches: Vec<SupervisorLaunch>,
        concurrency: LaunchConcurrency,
    ) -> Result<Vec<Result<(), LauncherTaskError>>, LauncherError> {
        if launches.len() > concurrency.get() {
            return Err(LauncherError::BatchLimit {
                actual: launches.len(),
                limit: concurrency.get(),
            });
        }
        let mut prepared = Vec::with_capacity(launches.len());
        let mut order = Vec::with_capacity(launches.len());
        let mut states = BTreeMap::new();
        for launch in launches {
            let task = self.allocate_task()?;
            let command =
                prepare_launcher_command(binary, &launch.snapshot, &launch.runtime_directory)?;
            order.push(task);
            states.insert(task, LaunchTaskState::default());
            prepared.push((task, command));
        }
        for (task, command) in prepared {
            self.client
                .spawn_task(task, command, SUPERVISOR_START_TIMEOUT)
                .await?;
        }
        if states.is_empty() {
            return Ok(Vec::new());
        }
        timeout(
            SUPERVISOR_START_TIMEOUT,
            self.wait_for_launch_batch(&mut states),
        )
        .await
        .map_err(|_| LauncherError::Timeout)??;
        order
            .into_iter()
            .map(|task| {
                states
                    .remove(&task)
                    .and_then(|state| state.outcome)
                    .ok_or_else(|| {
                        LauncherError::OperatingSystem(io::Error::other(
                            "completed launch task has no outcome",
                        ))
                    })
            })
            .collect()
    }

    fn allocate_task(&mut self) -> Result<BrokerTaskId, LauncherError> {
        let task = BrokerTaskId::new(self.next_task)
            .ok_or_else(|| io::Error::other("supervisor launcher task space exhausted"))?;
        self.next_task = self
            .next_task
            .checked_add(1)
            .ok_or_else(|| io::Error::other("supervisor launcher task counter overflow"))?;
        Ok(task)
    }

    async fn wait_for_launcher(&mut self, task: BrokerTaskId) -> Result<(), LauncherError> {
        loop {
            match self.client.next_event().await? {
                ProcessBrokerEvent::TaskStarted { task: current, .. } if current == task => {}
                ProcessBrokerEvent::TaskChild {
                    task: current,
                    event: ChildEvent::Exited { code: 0, .. },
                } if current == task => return Ok(()),
                ProcessBrokerEvent::TaskChild {
                    task: current,
                    event,
                } if current == task && event.is_terminal() => {
                    return Err(LauncherError::LauncherExited(event));
                }
                ProcessBrokerEvent::TaskSpawnFailed { task: current, .. } if current == task => {
                    return Err(LauncherError::OperatingSystem(io::Error::other(
                        "broker could not execute the immortal launcher",
                    )));
                }
                event => return Err(LauncherError::UnexpectedEvent(event)),
            }
        }
    }

    async fn wait_for_launch_batch(
        &mut self,
        states: &mut BTreeMap<BrokerTaskId, LaunchTaskState>,
    ) -> Result<(), LauncherError> {
        let mut remaining = states.len();
        while remaining != 0 {
            let event = self.client.next_event().await?;
            match &event {
                ProcessBrokerEvent::TaskStarted { task, .. } => {
                    let Some(state) = states.get_mut(task) else {
                        return Err(LauncherError::UnexpectedEvent(event));
                    };
                    if state.started || state.outcome.is_some() {
                        return Err(LauncherError::OperatingSystem(io::Error::other(
                            "duplicate or late launcher start event",
                        )));
                    }
                    state.started = true;
                }
                ProcessBrokerEvent::TaskChild {
                    task,
                    event: child_event,
                } if child_event.is_terminal() => {
                    let Some(state) = states.get_mut(task) else {
                        return Err(LauncherError::UnexpectedEvent(event));
                    };
                    if !state.started || state.outcome.is_some() {
                        return Err(LauncherError::OperatingSystem(io::Error::other(
                            "launcher terminal event violated task ordering",
                        )));
                    }
                    state.outcome = Some(match child_event {
                        ChildEvent::Exited { code: 0, .. } => Ok(()),
                        terminal => Err(LauncherTaskError::Exited(*terminal)),
                    });
                    remaining = remaining
                        .checked_sub(1)
                        .ok_or_else(|| io::Error::other("launcher completion counter underflow"))?;
                }
                ProcessBrokerEvent::TaskSpawnFailed { task, .. } => {
                    let Some(state) = states.get_mut(task) else {
                        return Err(LauncherError::UnexpectedEvent(event));
                    };
                    if state.started || state.outcome.is_some() {
                        return Err(LauncherError::OperatingSystem(io::Error::other(
                            "launcher spawn failure violated task ordering",
                        )));
                    }
                    state.outcome = Some(Err(LauncherTaskError::Spawn));
                    remaining = remaining
                        .checked_sub(1)
                        .ok_or_else(|| io::Error::other("launcher completion counter underflow"))?;
                }
                _ => return Err(LauncherError::UnexpectedEvent(event)),
            }
        }
        Ok(())
    }

    /// Stop and reap the otherwise childless launch broker.
    ///
    /// # Errors
    ///
    /// Returns a broker, timeout, unexpected-event, or broker-exit failure.
    pub async fn shutdown(mut self) -> Result<(), LauncherError> {
        let process = self.client.process();
        self.client.shutdown().await?;
        match timeout(SUPERVISOR_START_TIMEOUT, self.client.next_event()).await {
            Ok(Ok(ProcessBrokerEvent::ShutdownComplete)) => {}
            Ok(Ok(event)) => return Err(LauncherError::UnexpectedEvent(event)),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(LauncherError::Timeout),
        }
        drop(self.client);
        let event = wait_for_event(process)?;
        if matches!(event, ChildEvent::Exited { code: 0, .. }) {
            Ok(())
        } else {
            Err(LauncherError::LauncherExited(event))
        }
    }
}

fn prepare_launcher_command(
    binary: &Path,
    snapshot: &Path,
    runtime_directory: &Path,
) -> Result<ProcessCommand, LauncherError> {
    let binary_text = binary.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "immortal binary path is not UTF-8",
        )
    })?;
    let config = ServiceConfig::for_command(vec![binary_text.to_owned()])?;
    let mut command = ProcessCommand::from_service(&config, std::env::vars_os())?;
    command
        .argument("--config")
        .argument(snapshot.as_os_str())
        .argument("--control-dir")
        .argument(runtime_directory.as_os_str());
    Ok(command)
}

/// Scan top-level, non-hidden, regular `*.yml` definitions with bounded reads.
///
/// An invalid candidate is returned in [`ScanResult::problems`] while other
/// valid services remain available. Filesystem notifications should call this
/// function; they must not be interpreted as desired-state mutations directly.
///
/// # Errors
///
/// Returns an error only when the definitions directory itself cannot be read.
pub fn scan_directory(directory: &Path, limits: ScanLimits) -> Result<ScanResult, ScanError> {
    let directory = canonical_definitions_directory(directory)?;
    let entries = fs::read_dir(&directory).map_err(ScanError)?;
    let mut candidates = Vec::new();
    let mut problems = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                problems.push(ScanProblem {
                    path: directory.clone(),
                    kind: ScanProblemKind::Io(error),
                });
                continue;
            }
        };
        let path = entry.path();
        if !is_candidate(&path) {
            continue;
        }
        if candidates.len() >= limits.max_definitions {
            problems.push(ScanProblem {
                path,
                kind: ScanProblemKind::DefinitionLimit,
            });
            continue;
        }
        candidates.push(path);
    }
    candidates.sort();

    let mut definitions = BTreeMap::new();
    for path in candidates {
        let Some(name) = definition_name(&path) else {
            problems.push(ScanProblem {
                path,
                kind: ScanProblemKind::UnsafeName,
            });
            continue;
        };
        match read_definition(&path) {
            Ok(config) => {
                if definitions.contains_key(&name) {
                    problems.push(ScanProblem {
                        path,
                        kind: ScanProblemKind::DuplicateName,
                    });
                } else {
                    definitions.insert(name.clone(), Definition { name, path, config });
                }
            }
            Err(kind) => problems.push(ScanProblem { path, kind }),
        }
    }

    Ok(ScanResult {
        definitions,
        problems,
    })
}

/// Resolve a definitions directory once and reject a symlink or non-directory
/// final component.
///
/// # Errors
///
/// Returns an error when the path cannot be inspected or canonicalized, is not
/// a real directory, or changes identity during validation.
pub fn canonical_definitions_directory(directory: &Path) -> Result<PathBuf, ScanError> {
    let before = fs::symlink_metadata(directory).map_err(ScanError)?;
    if before.file_type().is_symlink() || !before.is_dir() {
        return Err(ScanError(io::Error::new(
            io::ErrorKind::InvalidInput,
            "definitions path must be a real directory, not a symlink",
        )));
    }
    let canonical = fs::canonicalize(directory).map_err(ScanError)?;
    let after = fs::metadata(&canonical).map_err(ScanError)?;
    if !after.is_dir() || file_identity(&before) != file_identity(&after) {
        return Err(ScanError(io::Error::new(
            io::ErrorKind::InvalidInput,
            "definitions directory changed during validation",
        )));
    }
    Ok(canonical)
}

fn is_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.starts_with('.') && path.extension().is_some_and(|extension| extension == "yml")
}

fn definition_name(path: &Path) -> Option<String> {
    let name = path.file_stem()?.to_str()?;
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    safe.then(|| name.to_owned())
}

fn read_definition(path: &Path) -> Result<ServiceConfig, ScanProblemKind> {
    let path_before = fs::symlink_metadata(path).map_err(ScanProblemKind::Io)?;
    if path_before.file_type().is_symlink() {
        return Err(ScanProblemKind::Symlink);
    }
    if !path_before.is_file() {
        return Err(ScanProblemKind::NotRegular);
    }

    let mut file = File::open(path).map_err(ScanProblemKind::Io)?;
    let before = file.metadata().map_err(ScanProblemKind::Io)?;
    if !before.is_file() {
        return Err(ScanProblemKind::NotRegular);
    }
    if before.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ScanProblemKind::Config(ConfigError::TooLarge {
            actual: before.len(),
        }));
    }

    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
    (&mut file)
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ScanProblemKind::Io)?;
    let file_after = file.metadata().map_err(ScanProblemKind::Io)?;
    let path_after = fs::symlink_metadata(path).map_err(ScanProblemKind::Io)?;
    if path_after.file_type().is_symlink()
        || metadata_changed(&before, &file_after)
        || metadata_changed(&before, &path_after)
    {
        return Err(ScanProblemKind::ChangedDuringRead);
    }
    parse_bytes_at(&bytes, path).map_err(ScanProblemKind::Config)
}

fn metadata_changed(before: &Metadata, after: &Metadata) -> bool {
    file_identity(before) != file_identity(after)
        || before.len() != after.len()
        || modified(before) != modified(after)
        || !after.is_file()
}

fn modified(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

/// Desired-state change computed from two valid semantic snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileAction {
    /// Definition became enabled and has no current supervisor.
    Start,
    /// Valid normalized configuration changed.
    Restart,
    /// Definition is disabled or was stably removed.
    Stop,
    /// Valid desired and current state are already equivalent.
    Keep,
}

/// Compute semantic desired-state action without relying on mtimes.
#[must_use]
pub fn compare(
    previous: Option<&ServiceConfig>,
    desired: Option<&ServiceConfig>,
) -> ReconcileAction {
    match (previous, desired) {
        (None, Some(config)) if config.enabled => ReconcileAction::Start,
        (Some(_), None) => ReconcileAction::Stop,
        (None, Some(_) | None) => ReconcileAction::Keep,
        (Some(old), Some(new)) if old == new => ReconcileAction::Keep,
        (Some(old), Some(new)) if old.enabled && !new.enabled => ReconcileAction::Stop,
        (Some(old), Some(new)) if !old.enabled && new.enabled => ReconcileAction::Start,
        (Some(_), Some(new)) if !new.enabled => ReconcileAction::Keep,
        (Some(_), Some(_)) => ReconcileAction::Restart,
    }
}

/// Merge a scan into last-known-good desired state.
///
/// Names with invalid replacement files retain their previous valid value.
/// Valid definitions replace prior values. Stable deletion handling remains a
/// caller policy because a single partial scan must not imply deletion.
#[must_use]
pub fn retain_last_known_good(
    previous: &BTreeMap<String, ServiceConfig>,
    scan: &ScanResult,
) -> BTreeMap<String, ServiceConfig> {
    let mut desired = previous.clone();
    desired.extend(
        scan.definitions
            .iter()
            .map(|(name, definition)| (name.clone(), definition.config.clone())),
    );
    desired
}

/// Persistent desired-state view built from authoritative directory scans.
///
/// Valid definitions replace older values immediately. Invalid replacements
/// retain their last-known-good value and count as present. A missing file must
/// remain absent for the configured number of consecutive scans before its
/// desired state is removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredStateTracker {
    desired: BTreeMap<String, ServiceConfig>,
    absent_scans: BTreeMap<String, usize>,
    deletion_confirmations: NonZeroUsize,
}

impl Default for DesiredStateTracker {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(DEFAULT_DELETION_CONFIRMATIONS).unwrap_or(NonZeroUsize::MIN))
    }
}

impl DesiredStateTracker {
    /// Construct a tracker with an explicit stable-deletion threshold.
    #[must_use]
    pub const fn new(deletion_confirmations: NonZeroUsize) -> Self {
        Self {
            desired: BTreeMap::new(),
            absent_scans: BTreeMap::new(),
            deletion_confirmations,
        }
    }

    /// Current last-known-good desired configurations.
    #[must_use]
    pub const fn desired(&self) -> &BTreeMap<String, ServiceConfig> {
        &self.desired
    }

    /// Forget a confirmed deletion only after the supervisor and applied state
    /// have been removed successfully.
    pub fn acknowledge_deletion(&mut self, name: &str) {
        if !self.desired.contains_key(name) {
            self.absent_scans.remove(name);
        }
    }

    /// Apply one complete scan and return one deterministic action per known name.
    ///
    /// Reapplying an identical scan yields `Keep`. A confirmed deletion yields
    /// `Stop` once in memory; a persisted unacknowledged deletion is replayed
    /// after restart so cleanup cannot be lost.
    pub fn apply(&mut self, scan: &ScanResult) -> BTreeMap<String, ReconcileAction> {
        let problem_names: BTreeSet<String> = scan
            .problems
            .iter()
            .filter_map(|problem| {
                is_candidate(&problem.path)
                    .then(|| definition_name(&problem.path))
                    .flatten()
            })
            .collect();
        let scan_incomplete = scan.problems.iter().any(|problem| {
            matches!(&problem.kind, ScanProblemKind::Io(_)) && !is_candidate(&problem.path)
        });
        let mut actions = BTreeMap::new();

        for (name, definition) in &scan.definitions {
            let action = compare(self.desired.get(name), Some(&definition.config));
            self.desired.insert(name.clone(), definition.config.clone());
            self.absent_scans.remove(name);
            actions.insert(name.clone(), action);
        }

        for name in &problem_names {
            self.absent_scans.remove(name);
            if self.desired.contains_key(name) {
                actions.entry(name.clone()).or_insert(ReconcileAction::Keep);
            }
        }

        let missing: Vec<String> = self
            .desired
            .keys()
            .filter(|name| !scan.definitions.contains_key(*name) && !problem_names.contains(*name))
            .cloned()
            .collect();
        for name in missing {
            if scan_incomplete {
                actions.insert(name, ReconcileAction::Keep);
                continue;
            }
            let count = self.absent_scans.entry(name.clone()).or_default();
            *count = count.saturating_add(1);
            if *count >= self.deletion_confirmations.get() {
                *count = self.deletion_confirmations.get();
                self.desired.remove(&name);
                actions.insert(name, ReconcileAction::Stop);
            } else {
                actions.insert(name, ReconcileAction::Keep);
            }
        }
        actions
    }
}

/// Deterministic groups of independent services which may start concurrently.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DependencyPlan {
    /// Ordered waves. Every dependency of a wave appears in an earlier wave.
    pub waves: Vec<Vec<String>>,
}

/// Invalid desired dependency graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DependencyError {
    /// Required definition is missing or explicitly disabled.
    Unavailable {
        /// Service with the requirement.
        service: String,
        /// Missing or disabled requirement.
        dependency: String,
    },
    /// Enabled definitions contain a dependency cycle.
    Cycle {
        /// Sorted services which could not be topologically ordered.
        services: Vec<String>,
    },
}

impl Display for DependencyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable {
                service,
                dependency,
            } => write!(
                formatter,
                "service `{service}` requires unavailable service `{dependency}`"
            ),
            Self::Cycle { services } => {
                write!(formatter, "dependency cycle among: {}", services.join(", "))
            }
        }
    }
}

impl Error for DependencyError {}

/// Validate enabled-service dependencies and compute concurrent start waves.
///
/// `requires` gates initial starts only. This plan intentionally says nothing
/// about cascading stops after a dependency later becomes unavailable.
///
/// # Errors
///
/// Returns an error for a missing/disabled dependency or any cycle.
pub fn dependency_plan(
    desired: &BTreeMap<String, ServiceConfig>,
) -> Result<DependencyPlan, DependencyError> {
    let enabled: BTreeSet<&str> = desired
        .iter()
        .filter_map(|(name, config)| config.enabled.then_some(name.as_str()))
        .collect();
    let mut remaining_requirements = BTreeMap::new();
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for (name, config) in desired.iter().filter(|(_, config)| config.enabled) {
        for dependency in &config.requires {
            if !enabled.contains(dependency.as_str()) {
                return Err(DependencyError::Unavailable {
                    service: name.clone(),
                    dependency: dependency.clone(),
                });
            }
            dependents
                .entry(dependency)
                .or_default()
                .insert(name.as_str());
        }
        remaining_requirements.insert(name.as_str(), config.requires.len());
    }

    let mut ready: Vec<&str> = remaining_requirements
        .iter()
        .filter_map(|(name, count)| (*count == 0).then_some(*name))
        .collect();
    let mut waves = Vec::new();
    let mut scheduled = 0_usize;
    while !ready.is_empty() {
        ready.sort_unstable();
        let wave = std::mem::take(&mut ready);
        scheduled = scheduled.saturating_add(wave.len());
        let mut next = BTreeSet::new();
        for completed in &wave {
            if let Some(children) = dependents.get(completed) {
                for child in children {
                    if let Some(count) = remaining_requirements.get_mut(child) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            next.insert(*child);
                        }
                    }
                }
            }
        }
        waves.push(wave.into_iter().map(ToOwned::to_owned).collect());
        ready.extend(next);
    }

    if scheduled == remaining_requirements.len() {
        Ok(DependencyPlan { waves })
    } else {
        let services = remaining_requirements
            .into_iter()
            .filter(|(_, count)| *count != 0)
            .map(|(name, _)| name.to_owned())
            .collect();
        Err(DependencyError::Cycle { services })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        error::Error,
        fs::{self, File},
        io,
        num::NonZeroUsize,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        DefinitionSnapshots, DependencyError, DesiredStateTracker, LaunchConcurrency,
        MAX_CONCURRENT_LAUNCHES, MAX_TRACKER_STATE_BYTES, ReconcileAction, ScanLimits, ScanProblem,
        ScanProblemKind, ScanResult, TRACKER_STATE_FILE, canonical_definitions_directory, compare,
        dependency_plan, metadata_changed, retain_last_known_good, scan_directory,
    };
    use crate::config::MAX_CONFIG_BYTES;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn launch_concurrency_rejects_zero_and_values_above_the_hard_bound() {
        assert!(LaunchConcurrency::new(0).is_err());
        assert_eq!(
            LaunchConcurrency::new(MAX_CONCURRENT_LAUNCHES).map(LaunchConcurrency::get),
            Ok(MAX_CONCURRENT_LAUNCHES)
        );
        assert!(LaunchConcurrency::new(MAX_CONCURRENT_LAUNCHES + 1).is_err());
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = Path::new("/tmp").join(format!(
                "immortal-reconcile-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(fs::canonicalize(path)?))
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
    fn scans_only_top_level_visible_yml_files() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(
            directory.path().join("notes.yaml"),
            "version: 2\ncommand: [/bin/false]\n",
        )?;
        fs::write(
            directory.path().join(".hidden.yml"),
            "version: 2\ncommand: [/bin/false]\n",
        )?;
        fs::create_dir(directory.path().join("nested"))?;
        fs::write(
            directory.path().join("nested/worker.yml"),
            "version: 2\ncommand: [/bin/false]\n",
        )?;

        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(scan.definitions.len(), 1);
        assert!(scan.definitions.contains_key("api"));
        assert!(scan.problems.is_empty());
        Ok(())
    }

    #[test]
    fn canonicalizes_a_real_definitions_directory() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let canonical = canonical_definitions_directory(directory.path())?;
        assert!(canonical.is_absolute());
        assert_eq!(canonical, fs::canonicalize(directory.path())?);
        Ok(())
    }

    #[test]
    fn normalized_snapshot_is_owner_only_and_atomically_replaceable() -> Result<(), Box<dyn Error>>
    {
        let runtime = TestDirectory::new()?;
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
        let snapshots = DefinitionSnapshots::open(runtime.path())?;
        let first = crate::config::parse_str("version: 2\ncommand: [/bin/true]\n")?;
        let path = snapshots.publish("api", &first)?;
        assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
        assert_eq!(crate::config::parse_file(&path)?, first);

        let second = crate::config::parse_str("version: 2\ncommand: [/bin/false]\n")?;
        assert_eq!(snapshots.publish("api", &second)?, path);
        assert_eq!(crate::config::parse_file(&path)?, second);
        snapshots.record_applied("api", &first)?;
        assert_eq!(snapshots.load_applied("api")?, Some(first));
        snapshots.remove_applied("api")?;
        assert_eq!(snapshots.load_applied("api")?, None);
        assert!(
            fs::read_dir(runtime.path().join(".definitions"))?.all(|entry| {
                entry.is_ok_and(|entry| entry.path().extension().is_none_or(|value| value != "tmp"))
            })
        );
        Ok(())
    }

    #[test]
    fn tracker_checkpoint_retries_confirmed_deletion_after_restart() -> Result<(), Box<dyn Error>> {
        let runtime = TestDirectory::new()?;
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
        let definitions = TestDirectory::new()?;
        let definition = definitions.path().join("api.yml");
        fs::write(&definition, "version: 2\ncommand: [/bin/true]\n")?;
        let snapshots = DefinitionSnapshots::open(runtime.path())?;
        let config = crate::config::parse_file(&definition)?;
        snapshots.record_applied("api", &config)?;
        assert_eq!(
            snapshots.load_tracker()?.desired().get("api"),
            Some(&config)
        );

        let mut tracker = DesiredStateTracker::default();
        let present = scan_directory(definitions.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&present).get("api"),
            Some(&ReconcileAction::Start)
        );
        snapshots.record_tracker(&tracker)?;

        fs::remove_file(&definition)?;
        let missing = scan_directory(definitions.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&missing).get("api"),
            Some(&ReconcileAction::Keep)
        );
        snapshots.record_tracker(&tracker)?;

        let mut restored = snapshots.load_tracker()?;
        assert!(restored.desired().contains_key("api"));
        assert_eq!(
            restored.apply(&missing).get("api"),
            Some(&ReconcileAction::Stop)
        );
        snapshots.record_tracker(&restored)?;

        let mut confirmed = snapshots.load_tracker()?;
        assert_eq!(
            confirmed.apply(&missing).get("api"),
            Some(&ReconcileAction::Stop)
        );
        snapshots.remove_applied("api")?;
        confirmed.acknowledge_deletion("api");
        snapshots.record_tracker(&confirmed)?;
        assert_eq!(snapshots.load_tracker()?, DesiredStateTracker::default());

        let state = snapshots.directory.join(TRACKER_STATE_FILE);
        assert_eq!(fs::metadata(state)?.permissions().mode() & 0o777, 0o600);
        Ok(())
    }

    #[test]
    fn tracker_checkpoint_rejects_malformed_unbounded_and_unsafe_state()
    -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let runtime = TestDirectory::new()?;
        fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
        let snapshots = DefinitionSnapshots::open(runtime.path())?;
        let state = snapshots.directory.join(TRACKER_STATE_FILE);

        fs::write(&state, "version\t1\n")?;
        fs::set_permissions(&state, fs::Permissions::from_mode(0o644))?;
        assert!(snapshots.load_tracker().is_err());
        fs::set_permissions(&state, fs::Permissions::from_mode(0o600))?;

        for malformed in [
            "",
            "version\t2\n",
            "version\t1\nmissing-count\n",
            "version\t1\nunsafe/name\t0\n",
            "version\t1\napi\t0\napi\t1\n",
            "version\t1\napi\t3\n",
        ] {
            fs::write(&state, malformed)?;
            assert!(snapshots.load_tracker().is_err(), "accepted {malformed:?}");
        }

        fs::write(&state, [0xff, 0xfe])?;
        assert!(snapshots.load_tracker().is_err());
        let file = File::create(&state)?;
        file.set_len((MAX_TRACKER_STATE_BYTES as u64).saturating_add(1))?;
        assert!(snapshots.load_tracker().is_err());

        drop(file);
        fs::remove_file(&state)?;
        let target = runtime.path().join("tracker-target");
        fs::write(&target, "version\t1\n")?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
        symlink(target, &state)?;
        assert!(snapshots.load_tracker().is_err());
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_definitions_directory() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let parent = TestDirectory::new()?;
        let target = parent.path().join("target");
        let link = parent.path().join("definitions");
        fs::create_dir(&target)?;
        symlink(&target, &link)?;
        assert!(canonical_definitions_directory(&link).is_err());
        Ok(())
    }

    #[test]
    fn file_identity_detects_same_sized_replacement() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let first = directory.path().join("first.yml");
        let second = directory.path().join("second.yml");
        fs::write(&first, b"1234")?;
        fs::write(&second, b"1234")?;
        assert!(metadata_changed(
            &fs::metadata(first)?,
            &fs::metadata(second)?
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_unsafe_names_without_losing_valid_services()
    -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new()?;
        let target = directory.path().join("target");
        fs::write(&target, "version: 2\ncommand: [/bin/true]\n")?;
        symlink(&target, directory.path().join("linked.yml"))?;
        fs::write(
            directory.path().join("unsafe name.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(
            directory.path().join("valid.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;

        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        assert!(scan.definitions.contains_key("valid"));
        assert!(
            scan.problems
                .iter()
                .any(|problem| matches!(problem.kind, ScanProblemKind::Symlink))
        );
        assert!(
            scan.problems
                .iter()
                .any(|problem| matches!(problem.kind, ScanProblemKind::UnsafeName))
        );
        Ok(())
    }

    #[test]
    fn isolates_invalid_files_and_enforces_definition_limit() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("a.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(directory.path().join("b.yml"), "not: [valid\n")?;
        fs::write(
            directory.path().join("c.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;

        let scan = scan_directory(directory.path(), ScanLimits { max_definitions: 2 })?;
        assert!(scan.definitions.len() <= 2);
        assert!(scan.problems.iter().any(|problem| matches!(
            problem.kind,
            ScanProblemKind::Config(_) | ScanProblemKind::DefinitionLimit
        )));
        assert!(
            scan.problems
                .iter()
                .any(|problem| matches!(problem.kind, ScanProblemKind::DefinitionLimit))
        );
        Ok(())
    }

    #[test]
    fn rejects_an_oversized_definition_without_allocating_it() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("large.yml");
        let file = fs::File::create(&path)?;
        file.set_len((MAX_CONFIG_BYTES as u64).saturating_add(1))?;

        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        assert!(scan.definitions.is_empty());
        assert!(scan.problems.iter().any(|problem| matches!(
            problem.kind,
            ScanProblemKind::Config(crate::config::ConfigError::TooLarge { .. })
        )));
        Ok(())
    }

    #[test]
    fn compares_semantics_instead_of_file_metadata() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        let first = scan_directory(directory.path(), ScanLimits::default())?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n# metadata-only change\n",
        )?;
        let second = scan_directory(directory.path(), ScanLimits::default())?;
        let old = first.definitions.get("api").map(|value| &value.config);
        let new = second.definitions.get("api").map(|value| &value.config);
        assert_eq!(compare(old, new), ReconcileAction::Keep);
        Ok(())
    }

    #[test]
    fn invalid_replacement_retains_last_known_good() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let valid = scan_directory(directory.path(), ScanLimits::default())?;
        let desired = retain_last_known_good(&BTreeMap::default(), &valid);
        fs::write(&path, "version: 2\ncommand: []\n")?;
        let invalid = scan_directory(directory.path(), ScanLimits::default())?;
        let retained = retain_last_known_good(&desired, &invalid);
        assert_eq!(retained, desired);
        Ok(())
    }

    #[test]
    fn desired_tracker_confirms_deletion_and_emits_stop_once() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::new(NonZeroUsize::new(2).ok_or("invalid limit")?);

        let present = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&present).get("api"),
            Some(&ReconcileAction::Start)
        );
        assert!(tracker.desired().contains_key("api"));

        fs::remove_file(&path)?;
        let first_missing = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&first_missing).get("api"),
            Some(&ReconcileAction::Keep)
        );
        assert!(tracker.desired().contains_key("api"));

        let confirmed_missing = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&confirmed_missing).get("api"),
            Some(&ReconcileAction::Stop)
        );
        assert!(!tracker.desired().contains_key("api"));
        assert!(tracker.apply(&confirmed_missing).is_empty());
        Ok(())
    }

    #[test]
    fn desired_tracker_never_treats_invalid_replacement_as_deletion() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::default();
        let valid = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&valid).get("api"),
            Some(&ReconcileAction::Start)
        );

        fs::write(&path, "version: 2\ncommand: []\n")?;
        for _ in 0..3 {
            let invalid = scan_directory(directory.path(), ScanLimits::default())?;
            assert_eq!(
                tracker.apply(&invalid).get("api"),
                Some(&ReconcileAction::Keep)
            );
            assert_eq!(
                tracker
                    .desired()
                    .get("api")
                    .map(|config| config.command.as_slice()),
                Some(["/bin/true".to_owned()].as_slice())
            );
        }
        Ok(())
    }

    #[test]
    fn desired_tracker_does_not_confirm_deletion_from_incomplete_scans()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::default();
        let valid = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&valid).get("api"),
            Some(&ReconcileAction::Start)
        );
        fs::remove_file(path)?;

        for _ in 0..3 {
            let incomplete = ScanResult {
                definitions: BTreeMap::new(),
                problems: vec![ScanProblem {
                    path: directory.path().to_owned(),
                    kind: ScanProblemKind::Io(io::Error::other("injected enumeration failure")),
                }],
            };
            assert_eq!(
                tracker.apply(&incomplete).get("api"),
                Some(&ReconcileAction::Keep)
            );
            assert!(tracker.desired().contains_key("api"));
        }

        let complete = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&complete).get("api"),
            Some(&ReconcileAction::Keep)
        );
        assert_eq!(
            tracker.apply(&complete).get("api"),
            Some(&ReconcileAction::Stop)
        );
        Ok(())
    }

    #[test]
    fn desired_tracker_reports_semantic_restart_and_disabled_stop() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        let mut tracker = DesiredStateTracker::default();
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let first = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&first).get("api"),
            Some(&ReconcileAction::Start)
        );

        fs::write(&path, "version: 2\ncommand: [/bin/false]\n")?;
        let changed = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&changed).get("api"),
            Some(&ReconcileAction::Restart)
        );
        assert_eq!(
            tracker.apply(&changed).get("api"),
            Some(&ReconcileAction::Keep)
        );

        fs::write(&path, "version: 2\nenabled: false\ncommand: [/bin/false]\n")?;
        let disabled = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&disabled).get("api"),
            Some(&ReconcileAction::Stop)
        );
        assert_eq!(
            tracker.apply(&disabled).get("api"),
            Some(&ReconcileAction::Keep)
        );

        fs::write(&path, "version: 2\ncommand: [/bin/false]\n")?;
        let reenabled = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&reenabled).get("api"),
            Some(&ReconcileAction::Start)
        );
        Ok(())
    }

    #[test]
    fn dependency_plan_groups_independent_services() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("database.yml"),
            "version: 2\ncommand: [database]\n",
        )?;
        fs::write(
            directory.path().join("cache.yml"),
            "version: 2\ncommand: [cache]\n",
        )?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [api]\nrequires: [database, cache]\n",
        )?;
        fs::write(
            directory.path().join("worker.yml"),
            "version: 2\ncommand: [worker]\nrequires: [database]\n",
        )?;
        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        let desired = retain_last_known_good(&BTreeMap::default(), &scan);

        assert_eq!(
            dependency_plan(&desired)?.waves,
            [
                vec!["cache".to_owned(), "database".to_owned()],
                vec!["api".to_owned(), "worker".to_owned()],
            ]
        );
        Ok(())
    }

    #[test]
    fn dependency_plan_rejects_missing_disabled_and_cycles() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [api]\nrequires: [database]\n",
        )?;
        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        let mut desired = retain_last_known_good(&BTreeMap::default(), &scan);
        assert!(matches!(
            dependency_plan(&desired),
            Err(DependencyError::Unavailable { .. })
        ));

        let mut database = desired
            .get("api")
            .ok_or_else(|| io::Error::other("api fixture missing"))?
            .clone();
        database.enabled = false;
        database.command = vec!["database".to_owned()];
        database.requires.clear();
        desired.insert("database".to_owned(), database);
        assert!(matches!(
            dependency_plan(&desired),
            Err(DependencyError::Unavailable { .. })
        ));

        let api = desired
            .get_mut("api")
            .ok_or_else(|| io::Error::other("api fixture missing"))?;
        api.requires = vec!["worker".to_owned()];
        let mut worker = api.clone();
        worker.command = vec!["worker".to_owned()];
        worker.requires = vec!["api".to_owned()];
        desired.insert("worker".to_owned(), worker);
        assert_eq!(
            dependency_plan(&desired),
            Err(DependencyError::Cycle {
                services: vec!["api".to_owned(), "worker".to_owned()]
            })
        );
        Ok(())
    }
}
