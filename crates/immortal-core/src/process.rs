//! Child process creation, daemonization, ownership, environment, and signals.
//!
//! Unix fork, session, daemon, and wait primitives come from the `fork` crate.
//! This module owns the supervisor-facing wrappers so syscall details do not
//! leak into the CLI crates or the rest of the domain model.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::{OsStr, OsString},
    fmt::{self, Display, Formatter},
    io,
    os::fd::{AsRawFd, OwnedFd, RawFd},
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    time::Duration,
};

use crate::config::{EnvironmentMode, ServiceConfig};

mod broker;
mod broker_protocol;

pub(crate) use broker::{
    BrokerLoggerId, BrokerLoggerPipeline, BrokerLoggingPlan, start_process_broker_with_logging,
};
pub use broker::{
    BrokerSignalScope, BrokerTaskId, ProcessBrokerClient, ProcessBrokerEndpoint,
    ProcessBrokerError, ProcessBrokerEvent, ReadinessFailure, start_process_broker,
};

/// Stable stage in the checked daemon startup sequence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonStage {
    FirstFork,
    CreateSession,
    SecondFork,
    CurrentDirectory,
    Descriptors,
    Initialization,
    Handshake,
    SignalState,
}

impl Display for DaemonStage {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::FirstFork => "first fork",
            Self::CreateSession => "session creation",
            Self::SecondFork => "second fork",
            Self::CurrentDirectory => "working-directory change",
            Self::Descriptors => "descriptor setup",
            Self::Initialization => "daemon initialization",
            Self::Handshake => "startup handshake",
            Self::SignalState => "signal-state reset",
        })
    }
}

/// Stable classification for checked daemon startup failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonFailure {
    OperatingSystem,
    TimedOut,
    Abandoned,
    InvalidHandshake,
}

/// Residual daemon ownership after fork's bounded cleanup attempt.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DaemonCleanup {
    intermediate: Option<ProcessId>,
    process_group: Option<ProcessGroupId>,
}

impl DaemonCleanup {
    /// Return an intermediate direct child which still requires reaping.
    #[must_use]
    pub const fn intermediate(self) -> Option<ProcessId> {
        self.intermediate
    }

    /// Return a daemon process group which may still require termination.
    #[must_use]
    pub const fn process_group(self) -> Option<ProcessGroupId> {
        self.process_group
    }

    /// Whether fork completed every cleanup obligation before returning.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.intermediate.is_none() && self.process_group.is_none()
    }
}

/// Failure to detach or initialize a checked daemon.
#[derive(Debug)]
pub struct DaemonError {
    stage: Option<DaemonStage>,
    failure: DaemonFailure,
    cleanup_pending: DaemonCleanup,
    source: Option<io::Error>,
}

impl DaemonError {
    /// Return the startup stage when the failure came from an OS operation.
    #[must_use]
    pub const fn stage(&self) -> Option<DaemonStage> {
        self.stage
    }

    /// Return the stable failure classification.
    #[must_use]
    pub const fn failure(&self) -> DaemonFailure {
        self.failure
    }

    /// Return residual process ownership after bounded cleanup.
    #[must_use]
    pub const fn cleanup_pending(&self) -> DaemonCleanup {
        self.cleanup_pending
    }

    /// Return the underlying OS error number when one was reported.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.source.as_ref().and_then(io::Error::raw_os_error)
    }
}

impl Display for DaemonError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match (self.failure, self.stage, self.source.as_ref()) {
            (DaemonFailure::OperatingSystem, Some(stage), Some(error)) => {
                write!(formatter, "daemon {stage} failed: {error}")
            }
            (DaemonFailure::OperatingSystem, Some(stage), None) => {
                write!(formatter, "daemon {stage} failed")
            }
            (DaemonFailure::OperatingSystem, None, _) => {
                formatter.write_str("daemon preparation failed")
            }
            (DaemonFailure::TimedOut, _, _) => formatter.write_str("daemon startup timed out"),
            (DaemonFailure::Abandoned, _, _) => {
                formatter.write_str("daemon abandoned its startup channel")
            }
            (DaemonFailure::InvalidHandshake, _, _) => {
                formatter.write_str("daemon returned an invalid startup handshake")
            }
        }
    }
}

impl Error for DaemonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

/// Detached daemon-side readiness token.
#[derive(Debug)]
pub struct DaemonStartup(fork::DaemonNotifier);

impl DaemonStartup {
    /// Tell the original invoker that Immortal's runtime is ready.
    ///
    /// # Errors
    ///
    /// Returns the startup-channel error when the invoker is no longer waiting.
    pub fn notify_ready(self) -> io::Result<()> {
        self.0.notify_ready()
    }

    /// Report initialization failure and terminate without inherited destructors.
    pub fn fail_and_exit(self, error: &io::Error) -> ! {
        self.0.fail_and_exit(error)
    }
}

/// Which process returned from checked daemonization.
#[derive(Debug)]
pub enum Daemonized {
    /// The original invoker after the detached supervisor reported ready.
    Parent {
        process: ProcessId,
        process_group: ProcessGroupId,
    },
    /// The detached supervisor which must initialize and report readiness.
    Daemon(DaemonStartup),
}

/// Detach before Tokio starts and retain a checked startup channel.
///
/// The supervisor changes to `/`, redirects all standard streams to `/dev/null`,
/// and resets portable signal state. Service paths and environment must be
/// materialized before calling this function.
///
/// # Errors
///
/// Returns preparation, fork/session, handshake, timeout, or cleanup failures.
pub fn daemonize(startup_timeout: Duration) -> Result<Daemonized, DaemonError> {
    let mut options = fork::DaemonOptions::new();
    options
        .current_directory("/")
        .map_err(|error| daemon_preparation_error(DaemonStage::CurrentDirectory, error))?
        .redirect_standard_io_to_null()
        .map_err(|error| daemon_preparation_error(DaemonStage::Descriptors, error))?;
    match fork::checked_daemon(options, startup_timeout).map_err(daemon_error_from_fork)? {
        fork::CheckedDaemon::Parent(process) => Ok(Daemonized::Parent {
            process: ProcessId(process.process().get()),
            process_group: ProcessGroupId(process.process_group().get()),
        }),
        fork::CheckedDaemon::Daemon(notifier) => Ok(Daemonized::Daemon(DaemonStartup(notifier))),
    }
}

fn daemon_preparation_error(stage: DaemonStage, error: io::Error) -> DaemonError {
    DaemonError {
        stage: Some(stage),
        failure: DaemonFailure::OperatingSystem,
        cleanup_pending: DaemonCleanup::default(),
        source: Some(error),
    }
}

fn daemon_error_from_fork(error: fork::DaemonError) -> DaemonError {
    let cleanup = error.cleanup_pending();
    let cleanup_pending = DaemonCleanup {
        intermediate: cleanup
            .intermediate()
            .map(|process| ProcessId(process.get())),
        process_group: cleanup
            .process_group()
            .map(|group| ProcessGroupId(group.get())),
    };
    match error {
        fork::DaemonError::OperatingSystem { stage, error, .. } => DaemonError {
            stage: Some(daemon_stage_from_fork(stage)),
            failure: DaemonFailure::OperatingSystem,
            cleanup_pending,
            source: Some(error),
        },
        fork::DaemonError::TimedOut { .. } => DaemonError {
            stage: None,
            failure: DaemonFailure::TimedOut,
            cleanup_pending,
            source: None,
        },
        fork::DaemonError::Abandoned { .. } => DaemonError {
            stage: None,
            failure: DaemonFailure::Abandoned,
            cleanup_pending,
            source: None,
        },
        fork::DaemonError::InvalidHandshake { .. } => DaemonError {
            stage: None,
            failure: DaemonFailure::InvalidHandshake,
            cleanup_pending,
            source: None,
        },
    }
}

const fn daemon_stage_from_fork(stage: fork::DaemonStage) -> DaemonStage {
    match stage {
        fork::DaemonStage::FirstFork => DaemonStage::FirstFork,
        fork::DaemonStage::CreateSession => DaemonStage::CreateSession,
        fork::DaemonStage::SecondFork => DaemonStage::SecondFork,
        fork::DaemonStage::CurrentDirectory => DaemonStage::CurrentDirectory,
        fork::DaemonStage::Descriptors => DaemonStage::Descriptors,
        fork::DaemonStage::Initialization => DaemonStage::Initialization,
        fork::DaemonStage::Handshake => DaemonStage::Handshake,
        fork::DaemonStage::SignalState => DaemonStage::SignalState,
    }
}

/// Deterministic environment passed to a service or lifecycle hook.
pub type ProcessEnvironment = BTreeMap<OsString, OsString>;

/// Supplementary-group behavior resolved before daemonization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupplementaryGroups {
    /// Preserve the caller's groups when an unprivileged supervisor remains the same user.
    Preserve,
    /// Replace the complete supplementary group list before setting GID and UID.
    Set(Vec<libc::gid_t>),
}

/// Numeric credentials transported to the single-threaded process broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCredentials {
    user: libc::uid_t,
    group: libc::gid_t,
    supplementary_groups: SupplementaryGroups,
}

impl ProcessCredentials {
    /// Construct an explicit numeric identity transition.
    #[must_use]
    pub const fn new(
        user: libc::uid_t,
        group: libc::gid_t,
        supplementary_groups: SupplementaryGroups,
    ) -> Self {
        Self {
            user,
            group,
            supplementary_groups,
        }
    }

    /// Return the target UID.
    #[must_use]
    pub const fn user(&self) -> libc::uid_t {
        self.user
    }

    /// Return the target primary GID.
    #[must_use]
    pub const fn group(&self) -> libc::gid_t {
        self.group
    }

    /// Return the resolved supplementary-group policy.
    #[must_use]
    pub const fn supplementary_groups(&self) -> &SupplementaryGroups {
        &self.supplementary_groups
    }
}

/// Fully materialized direct-exec request passed to the process broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessCommand {
    program: OsString,
    arguments: Vec<OsString>,
    environment: ProcessEnvironment,
    working_directory: Option<PathBuf>,
    credentials: Option<ProcessCredentials>,
}

impl ProcessCommand {
    /// Build a command from one validated service and an explicit environment snapshot.
    ///
    /// The first configured argument is the executable. It is deliberately not
    /// resolved through `PATH`; configuration resolution must provide the exact
    /// executable expected by the operator.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if the validated configuration unexpectedly has
    /// no executable.
    pub fn from_service(
        config: &ServiceConfig,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
    ) -> io::Result<Self> {
        let (program, arguments) = config.command.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "service command is empty")
        })?;
        let environment = resolve_environment(config, inherited);
        Ok(Self {
            program: resolve_program(
                OsStr::new(program),
                &environment,
                config.working_directory.as_deref(),
            )?,
            arguments: arguments.iter().map(OsString::from).collect(),
            environment,
            working_directory: config.working_directory.clone(),
            credentials: resolve_credentials(config.user.as_deref())?,
        })
    }

    /// Build a lifecycle command with the service's resolved execution context.
    ///
    /// The environment, working directory, and credentials are copied once
    /// during supervisor preparation. Repeated attempts then use the same
    /// deterministic inputs as the service command.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when the argv is empty, or `NotFound` when a bare
    /// executable cannot be resolved from the service environment.
    pub(crate) fn from_lifecycle(command: &[String], service: &Self) -> io::Result<Self> {
        let (program, arguments) = command.split_first().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "lifecycle command is empty")
        })?;
        Self::from_lifecycle_os(
            OsStr::new(program),
            arguments.iter().map(OsString::from).collect(),
            service,
        )
    }

    pub(crate) fn from_lifecycle_os(
        program: &OsStr,
        arguments: Vec<OsString>,
        service: &Self,
    ) -> io::Result<Self> {
        Ok(Self {
            program: resolve_program(
                program,
                &service.environment,
                service.working_directory.as_deref(),
            )?,
            arguments,
            environment: service.environment.clone(),
            working_directory: service.working_directory.clone(),
            credentials: service.credentials.clone(),
        })
    }

    /// Build an explicit command, primarily for hooks and lifecycle contracts.
    #[must_use]
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            arguments: Vec::new(),
            environment: ProcessEnvironment::new(),
            working_directory: None,
            credentials: None,
        }
    }

    /// Append one direct argument without shell interpretation.
    pub fn argument(&mut self, argument: impl Into<OsString>) -> &mut Self {
        self.arguments.push(argument.into());
        self
    }

    /// Replace the complete environment passed to the child.
    pub fn environment(&mut self, environment: ProcessEnvironment) -> &mut Self {
        self.environment = environment;
        self
    }

    /// Insert one broker-owned environment field after command materialization.
    pub(crate) fn environment_variable(
        &mut self,
        key: impl Into<OsString>,
        value: impl Into<OsString>,
    ) {
        self.environment.insert(key.into(), value.into());
    }

    /// Set the directory entered immediately before execution.
    pub fn working_directory(&mut self, directory: impl Into<PathBuf>) -> &mut Self {
        self.working_directory = Some(directory.into());
        self
    }

    /// Apply an already-resolved numeric identity to the child.
    pub fn credentials(&mut self, credentials: ProcessCredentials) -> &mut Self {
        self.credentials = Some(credentials);
        self
    }

    /// Return the exact executable path.
    #[must_use]
    pub fn program(&self) -> &OsStr {
        &self.program
    }

    /// Return the direct argument vector excluding argv zero.
    #[must_use]
    pub fn arguments(&self) -> &[OsString] {
        &self.arguments
    }

    /// Return the complete child environment.
    #[must_use]
    pub fn resolved_environment(&self) -> &ProcessEnvironment {
        &self.environment
    }

    /// Return the requested child working directory.
    #[must_use]
    pub fn requested_working_directory(&self) -> Option<&std::path::Path> {
        self.working_directory.as_deref()
    }

    /// Return the requested numeric identity transition.
    #[must_use]
    pub const fn requested_credentials(&self) -> Option<&ProcessCredentials> {
        self.credentials.as_ref()
    }
}

fn resolve_credentials(user: Option<&str>) -> io::Result<Option<ProcessCredentials>> {
    let Some(user) = user else {
        return Ok(None);
    };
    let identity = crate::platform::resolve_account(user)?;
    let effective_user = nix::unistd::geteuid().as_raw();
    let effective_group = nix::unistd::getegid().as_raw();
    let supplementary_groups = if effective_user == 0 {
        SupplementaryGroups::Set(identity.supplementary_groups)
    } else if identity.user == effective_user && identity.group == effective_group {
        SupplementaryGroups::Preserve
    } else {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "an unprivileged supervisor may only select its current account and primary group",
        ));
    };
    Ok(Some(ProcessCredentials::new(
        identity.user,
        identity.group,
        supplementary_groups,
    )))
}

fn resolve_program(
    program: &OsStr,
    environment: &ProcessEnvironment,
    working_directory: Option<&std::path::Path>,
) -> io::Result<OsString> {
    if program.as_encoded_bytes().contains(&b'/') {
        return Ok(program.to_os_string());
    }
    let path = environment.get(OsStr::new("PATH")).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "bare executable requires PATH in the resolved environment",
        )
    })?;
    let base =
        working_directory.map_or_else(std::env::current_dir, |path| Ok(path.to_path_buf()))?;
    for directory in std::env::split_paths(path) {
        let directory = if directory.as_os_str().is_empty() {
            base.clone()
        } else if directory.is_absolute() {
            directory
        } else {
            base.join(directory)
        };
        let candidate = directory.join(program);
        if candidate
            .metadata()
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        {
            return Ok(candidate.into_os_string());
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!(
            "executable `{}` was not found in PATH",
            program.to_string_lossy()
        ),
    ))
}

/// Resolve the process environment without reading global state implicitly.
///
/// In inherited mode, entries are copied in iterator order and later duplicate
/// keys replace earlier ones. Configured UTF-8 values are then applied last. In
/// clear mode, only configured values are present. A caller can therefore take
/// one explicit snapshot of `std::env::vars_os()` before daemonization and use
/// the same inputs for every generation.
#[must_use]
pub fn resolve_environment(
    config: &ServiceConfig,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> ProcessEnvironment {
    let mut resolved = if config.environment_mode == EnvironmentMode::Inherit {
        inherited.into_iter().collect()
    } else {
        ProcessEnvironment::new()
    };
    resolved.extend(
        config
            .environment
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    resolved
}

/// Checked positive process identifier valid only while Immortal owns the child.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessId(i32);

impl ProcessId {
    /// Construct a process identifier only when the operating-system value is positive.
    #[must_use]
    pub const fn new(raw: i32) -> Option<Self> {
        if raw > 0 { Some(Self(raw)) } else { None }
    }

    /// Return the positive operating-system value for status and PID-file output.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl Display for ProcessId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

/// A raw process identifier was zero or negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProcessId(i32);

impl Display for InvalidProcessId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "process ID must be positive, got {}", self.0)
    }
}

impl Error for InvalidProcessId {}

impl TryFrom<i32> for ProcessId {
    type Error = InvalidProcessId;

    fn try_from(raw: i32) -> Result<Self, Self::Error> {
        Self::new(raw).ok_or(InvalidProcessId(raw))
    }
}

/// Checked positive process-group identifier owned by one service generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessGroupId(i32);

impl ProcessGroupId {
    const fn new(raw: i32) -> Option<Self> {
        if raw > 0 { Some(Self(raw)) } else { None }
    }

    /// Return the positive operating-system value for diagnostics.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl Display for ProcessGroupId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

/// A raw process-group identifier was zero or negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProcessGroupId(i32);

impl Display for InvalidProcessGroupId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "process-group ID must be positive, got {}",
            self.0
        )
    }
}

impl Error for InvalidProcessGroupId {}

impl TryFrom<i32> for ProcessGroupId {
    type Error = InvalidProcessGroupId;

    fn try_from(raw: i32) -> Result<Self, Self::Error> {
        Self::new(raw).ok_or(InvalidProcessGroupId(raw))
    }
}

/// Explicit signal target; raw negative PID conventions never cross this boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalTarget {
    /// Deliver to exactly one owned child.
    Process(ProcessId),
    /// Deliver to every current member of one owned generation group.
    Group(ProcessGroupId),
}

/// Portable signal vocabulary used by Immortal's process executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Quit,
    Kill,
    Alarm,
    Terminate,
    Stop,
    Continue,
    User1,
    User2,
    TerminalInput,
    TerminalOutput,
    WindowChange,
}

impl ProcessSignal {
    const fn into_fork(self) -> fork::Signal {
        match self {
            Self::Hangup => fork::Signal::HUP,
            Self::Interrupt => fork::Signal::INT,
            Self::Quit => fork::Signal::QUIT,
            Self::Kill => fork::Signal::KILL,
            Self::Alarm => fork::Signal::ALRM,
            Self::Terminate => fork::Signal::TERM,
            Self::Stop => fork::Signal::STOP,
            Self::Continue => fork::Signal::CONT,
            Self::User1 => fork::Signal::USR1,
            Self::User2 => fork::Signal::USR2,
            Self::TerminalInput => fork::Signal::TTIN,
            Self::TerminalOutput => fork::Signal::TTOU,
            Self::WindowChange => fork::Signal::WINCH,
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Quit => 3,
            Self::Kill => 4,
            Self::Alarm => 5,
            Self::Terminate => 6,
            Self::Stop => 7,
            Self::Continue => 8,
            Self::User1 => 9,
            Self::User2 => 10,
            Self::TerminalInput => 11,
            Self::TerminalOutput => 12,
            Self::WindowChange => 13,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Hangup),
            2 => Some(Self::Interrupt),
            3 => Some(Self::Quit),
            4 => Some(Self::Kill),
            5 => Some(Self::Alarm),
            6 => Some(Self::Terminate),
            7 => Some(Self::Stop),
            8 => Some(Self::Continue),
            9 => Some(Self::User1),
            10 => Some(Self::User2),
            11 => Some(Self::TerminalInput),
            12 => Some(Self::TerminalOutput),
            13 => Some(Self::WindowChange),
            _ => None,
        }
    }
}

/// Deliver one checked signal to an explicitly typed target.
///
/// # Errors
///
/// Returns the operating-system signal-delivery error from the canonical fork boundary.
pub fn signal(target: SignalTarget, signal: ProcessSignal) -> io::Result<()> {
    match target {
        SignalTarget::Process(process) => fork::signal_process(
            fork::ProcessId::try_from(process.get())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
            signal.into_fork(),
        ),
        SignalTarget::Group(group) => fork::signal_process_group(
            fork::ProcessGroupId::try_from(group.get())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
            signal.into_fork(),
        ),
    }
}

/// Successfully executed direct child and its dedicated generation group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnedProcess {
    process: ProcessId,
    group: ProcessGroupId,
}

/// One owned descriptor explicitly permitted to survive child execution.
///
/// Descriptors not present in this plan are closed by the canonical `fork`
/// boundary before `execve`. A descriptor may retain its current number or be
/// mapped to a deliberate child-facing number.
#[derive(Debug)]
pub struct ProcessDescriptor {
    source: OwnedFd,
    target: RawFd,
}

impl ProcessDescriptor {
    /// Preserve an owned descriptor at its current number in the child.
    #[must_use]
    pub fn inherit(source: OwnedFd) -> Self {
        let target = source.as_raw_fd();
        Self { source, target }
    }

    /// Map an owned descriptor to an explicit child-facing number.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when the requested descriptor number is negative.
    pub fn map(source: OwnedFd, target: RawFd) -> io::Result<Self> {
        if target < 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "child descriptor target must not be negative",
            ));
        }
        Ok(Self { source, target })
    }
}

impl SpawnedProcess {
    /// Return the main direct child.
    #[must_use]
    pub const fn process(self) -> ProcessId {
        self.process
    }

    /// Return the process group owned by this generation.
    #[must_use]
    pub const fn group(self) -> ProcessGroupId {
        self.group
    }
}

/// Stable stage at which direct process creation failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnStage {
    Specification,
    Fork,
    ProcessGroup,
    CurrentDirectory,
    DescriptorMapping,
    Execute,
    StartupHandshake,
    Identity,
    SignalState,
}

impl Display for SpawnStage {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Specification => "command specification",
            Self::Fork => "fork",
            Self::ProcessGroup => "process-group setup",
            Self::CurrentDirectory => "working-directory change",
            Self::DescriptorMapping => "descriptor mapping",
            Self::Execute => "execution",
            Self::StartupHandshake => "startup handshake",
            Self::Identity => "identity transition",
            Self::SignalState => "signal-state reset",
        })
    }
}

/// Stable classification for a failed process creation attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpawnFailure {
    OperatingSystem,
    TimedOut,
    InvalidHandshake,
    InvalidForkContract,
}

/// Failure to prepare, fork, or execute one process generation.
#[derive(Debug)]
pub struct SpawnError {
    stage: SpawnStage,
    failure: SpawnFailure,
    cleanup_pending: Option<ProcessId>,
    source: Option<io::Error>,
}

impl SpawnError {
    /// Return the stable failure stage.
    #[must_use]
    pub const fn stage(&self) -> SpawnStage {
        self.stage
    }

    /// Return the stable failure classification.
    #[must_use]
    pub const fn failure(&self) -> SpawnFailure {
        self.failure
    }

    /// Return a killed but not yet reaped child retained as a broker obligation.
    #[must_use]
    pub const fn cleanup_pending(&self) -> Option<ProcessId> {
        self.cleanup_pending
    }

    /// Return the operating-system error number when one was reported.
    #[must_use]
    pub fn raw_os_error(&self) -> Option<i32> {
        self.source.as_ref().and_then(io::Error::raw_os_error)
    }
}

impl Display for SpawnError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match (&self.failure, &self.source) {
            (SpawnFailure::OperatingSystem, Some(error)) => {
                write!(formatter, "child {} failed: {error}", self.stage)
            }
            (SpawnFailure::TimedOut, _) => {
                write!(formatter, "child {} timed out", self.stage)
            }
            (SpawnFailure::InvalidHandshake, _) => {
                formatter.write_str("child returned an invalid startup handshake")
            }
            (SpawnFailure::InvalidForkContract, _) => {
                formatter.write_str("fork returned a child without its requested process group")
            }
            (SpawnFailure::OperatingSystem, None) => {
                write!(formatter, "child {} failed", self.stage)
            }
        }
    }
}

impl Error for SpawnError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.source
            .as_ref()
            .map(|error| error as &(dyn Error + 'static))
    }
}

/// Prepare and execute one command in a new process group.
///
/// This blocking mechanism is intended to run only inside Immortal's
/// single-threaded process broker. Success means `execve` completed before the
/// bounded startup deadline.
///
/// # Errors
///
/// Returns a stable preparation or fork failure while retaining any child that
/// still requires broker cleanup.
pub fn spawn(
    command: ProcessCommand,
    startup_timeout: Duration,
) -> Result<SpawnedProcess, SpawnError> {
    spawn_with_descriptors(command, startup_timeout, [])
}

/// Prepare and execute one command with an explicit descriptor allow-list.
///
/// Only standard input/output/error and the descriptors supplied here can
/// survive successful execution. Descriptor targets must be unique.
///
/// # Errors
///
/// Returns a stable preparation or fork failure while retaining any child that
/// still requires broker cleanup.
pub fn spawn_with_descriptors(
    command: ProcessCommand,
    startup_timeout: Duration,
    descriptors: impl IntoIterator<Item = ProcessDescriptor>,
) -> Result<SpawnedProcess, SpawnError> {
    let mut prepared = fork::PreparedCommand::new(&command.program).map_err(specification_error)?;
    for argument in command.arguments {
        prepared.arg(argument).map_err(specification_error)?;
    }
    prepared.clear_environment();
    for (key, value) in command.environment {
        prepared
            .environment(key, value)
            .map_err(specification_error)?;
    }
    if let Some(directory) = command.working_directory {
        prepared
            .current_directory(directory)
            .map_err(specification_error)?;
    }
    if let Some(credentials) = command.credentials {
        let supplementary_groups = match credentials.supplementary_groups {
            SupplementaryGroups::Preserve => fork::SupplementaryGroups::Preserve,
            SupplementaryGroups::Set(groups) => fork::SupplementaryGroups::Set(groups),
        };
        prepared.credentials(fork::ProcessCredentials::new(
            credentials.user,
            credentials.group,
            supplementary_groups,
        ));
    }
    for descriptor in descriptors {
        prepared
            .map_descriptor(descriptor.source, descriptor.target)
            .map_err(specification_error)?;
    }
    prepared.process_group(fork::ProcessGroup::New);
    let child = prepared
        .spawn(startup_timeout)
        .map_err(spawn_error_from_fork)?;
    let process = ProcessId(child.process().get());
    let Some(group) = child.process_group() else {
        let _ = fork::signal_process(child.process(), fork::Signal::KILL);
        let cleanup_pending = match fork::wait_event(child.process()) {
            Ok(_) => None,
            Err(_) => Some(process),
        };
        return Err(SpawnError {
            stage: SpawnStage::ProcessGroup,
            failure: SpawnFailure::InvalidForkContract,
            cleanup_pending,
            source: None,
        });
    };
    Ok(SpawnedProcess {
        process,
        group: ProcessGroupId(group.get()),
    })
}

fn specification_error(error: io::Error) -> SpawnError {
    SpawnError {
        stage: SpawnStage::Specification,
        failure: SpawnFailure::OperatingSystem,
        cleanup_pending: None,
        source: Some(error),
    }
}

fn spawn_error_from_fork(error: fork::SpawnError) -> SpawnError {
    let cleanup_pending = error
        .cleanup_pending()
        .map(|process| ProcessId(process.get()));
    match error {
        fork::SpawnError::OperatingSystem { stage, error, .. } => SpawnError {
            stage: spawn_stage_from_fork(stage),
            failure: SpawnFailure::OperatingSystem,
            cleanup_pending,
            source: Some(error),
        },
        fork::SpawnError::TimedOut { .. } => SpawnError {
            stage: SpawnStage::StartupHandshake,
            failure: SpawnFailure::TimedOut,
            cleanup_pending,
            source: None,
        },
        fork::SpawnError::InvalidHandshake { .. } => SpawnError {
            stage: SpawnStage::StartupHandshake,
            failure: SpawnFailure::InvalidHandshake,
            cleanup_pending,
            source: None,
        },
    }
}

const fn spawn_stage_from_fork(stage: fork::SpawnStage) -> SpawnStage {
    match stage {
        fork::SpawnStage::Fork => SpawnStage::Fork,
        fork::SpawnStage::ProcessGroup => SpawnStage::ProcessGroup,
        fork::SpawnStage::CurrentDirectory => SpawnStage::CurrentDirectory,
        fork::SpawnStage::DescriptorDuplication => SpawnStage::DescriptorMapping,
        fork::SpawnStage::Execute => SpawnStage::Execute,
        fork::SpawnStage::StartupHandshake => SpawnStage::StartupHandshake,
        fork::SpawnStage::Identity => SpawnStage::Identity,
        fork::SpawnStage::SignalState => SpawnStage::SignalState,
    }
}

/// One state change drained from the canonical `fork` wait boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildEvent {
    /// The child exited normally with an eight-bit status.
    Exited { pid: ProcessId, code: u8 },
    /// The child was terminated by a signal.
    Signaled { pid: ProcessId, signal: u8 },
    /// The child was stopped but remains owned and waitable.
    Stopped { pid: ProcessId, signal: u8 },
    /// A stopped child resumed execution.
    Continued { pid: ProcessId },
}

impl ChildEvent {
    /// Return the child associated with this event.
    #[must_use]
    pub const fn pid(self) -> ProcessId {
        match self {
            Self::Exited { pid, .. }
            | Self::Signaled { pid, .. }
            | Self::Stopped { pid, .. }
            | Self::Continued { pid } => pid,
        }
    }

    /// Whether this event permanently reaped the child.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Signaled { .. })
    }

    /// Convert a terminal event into restart-policy input.
    #[must_use]
    pub const fn terminal_result(self) -> Option<crate::supervisor::ChildResult> {
        match self {
            Self::Exited { code, .. } => Some(crate::supervisor::ChildResult::Exited(code)),
            Self::Signaled { signal, .. } => Some(crate::supervisor::ChildResult::Signaled(signal)),
            Self::Stopped { .. } | Self::Continued { .. } => None,
        }
    }
}

/// Drain one pending child state change without blocking.
///
/// The process broker must call this repeatedly after a coalesced `SIGCHLD`
/// until it returns `Ok(None)` or the OS reports that no children remain.
///
/// # Errors
///
/// Returns an operating-system wait error or invalid event data from `fork`.
pub fn reap_any_event() -> io::Result<Option<ChildEvent>> {
    fork::wait_any_event_nohang()?
        .map(child_event_from_fork)
        .transpose()
}

/// Wait for one state change from an exact direct child.
///
/// This blocking operation is used to reap the broker itself after the Tokio
/// supervisor runtime has shut down.
///
/// # Errors
///
/// Returns an operating-system wait error or invalid event data from `fork`.
pub fn wait_for_event(process: ProcessId) -> io::Result<ChildEvent> {
    let process = fork::ProcessId::try_from(process.get())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    child_event_from_fork(fork::wait_event(process)?)
}

fn child_event_from_fork(event: fork::ChildEvent) -> io::Result<ChildEvent> {
    let pid = ProcessId::try_from(event.pid().get())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    match event {
        fork::ChildEvent::Exited { code, .. } => Ok(ChildEvent::Exited { pid, code }),
        fork::ChildEvent::Signalled { signal, .. } => Ok(ChildEvent::Signaled {
            pid,
            signal: signal_number(signal)?,
        }),
        fork::ChildEvent::Stopped { signal, .. } => Ok(ChildEvent::Stopped {
            pid,
            signal: signal_number(signal)?,
        }),
        fork::ChildEvent::Continued { .. } => Ok(ChildEvent::Continued { pid }),
    }
}

fn signal_number(signal: fork::Signal) -> io::Result<u8> {
    u8::try_from(signal.get()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "signal number is outside Immortal's portable representation",
        )
    })
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        ffi::{OsStr, OsString},
        io,
        time::Duration,
    };

    use crate::config::{EnvironmentMode, ServiceConfig, parse_str};

    use super::{
        ChildEvent, ProcessCommand, ProcessEnvironment, ProcessId, ProcessSignal, SpawnFailure,
        SpawnStage, child_event_from_fork, resolve_environment, spawn,
    };

    #[test]
    fn configured_values_override_one_explicit_inherited_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_str(
            "version: 2\ncommand: [service]\nenvironment:\n  KEEP: configured\n  NEW: value\n",
        )?;
        let environment = resolve_environment(
            &config,
            [
                (OsString::from("KEEP"), OsString::from("old")),
                (OsString::from("BASE"), OsString::from("base")),
            ],
        );
        assert_eq!(
            environment.get(OsStr::new("KEEP")),
            Some(&OsString::from("configured"))
        );
        assert_eq!(
            environment.get(OsStr::new("BASE")),
            Some(&OsString::from("base"))
        );
        assert_eq!(
            environment.get(OsStr::new("NEW")),
            Some(&OsString::from("value"))
        );
        Ok(())
    }

    #[test]
    fn clear_mode_discards_every_inherited_entry() -> Result<(), Box<dyn Error>> {
        let mut config = parse_str("version: 2\ncommand: [service]\n")?;
        config.environment_mode = EnvironmentMode::Clear;
        config
            .environment
            .insert("ONLY".to_owned(), "configured".to_owned());
        let environment = resolve_environment(
            &config,
            [(OsString::from("SECRET"), OsString::from("inherited"))],
        );
        assert_eq!(environment.len(), 1);
        assert_eq!(
            environment.get(OsStr::new("ONLY")),
            Some(&OsString::from("configured"))
        );
        Ok(())
    }

    #[test]
    fn service_command_materializes_direct_execution_inputs() -> Result<(), Box<dyn Error>> {
        let config =
            parse_str("version: 2\ncommand: [/bin/sleep, '5']\nenvironment:\n  MODE: test\n")?;
        let command = ProcessCommand::from_service(
            &config,
            [(OsString::from("INHERITED"), OsString::from("yes"))],
        )?;
        assert_eq!(command.program(), OsStr::new("/bin/sleep"));
        assert_eq!(command.arguments(), [OsString::from("5")]);
        assert_eq!(
            command.resolved_environment().get(OsStr::new("MODE")),
            Some(&OsString::from("test"))
        );
        assert_eq!(
            command.resolved_environment().get(OsStr::new("INHERITED")),
            Some(&OsString::from("yes"))
        );
        assert_eq!(command.requested_working_directory(), None);
        Ok(())
    }

    #[test]
    fn configured_account_is_resolved_before_broker_start() -> Result<(), Box<dyn Error>> {
        let effective = nix::unistd::geteuid();
        let account = nix::unistd::User::from_uid(effective)?.ok_or_else(|| {
            io::Error::other("effective account is absent from the user database")
        })?;
        let mut config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
        config.user = Some(account.name);
        let command = ProcessCommand::from_service(&config, ProcessEnvironment::new())?;
        let credentials = command
            .requested_credentials()
            .ok_or_else(|| io::Error::other("configured account was not materialized"))?;
        assert_eq!(credentials.user(), effective.as_raw());
        assert_eq!(credentials.group(), account.gid.as_raw());
        Ok(())
    }

    #[test]
    fn unknown_account_fails_before_broker_start() -> Result<(), Box<dyn Error>> {
        let mut config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
        config.user = Some("immortal-account-that-must-not-exist-7f9b".to_owned());
        let error = ProcessCommand::from_service(&config, ProcessEnvironment::new())
            .err()
            .ok_or_else(|| io::Error::other("unknown account unexpectedly resolved"))?;
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn bare_service_program_is_resolved_before_broker_start() -> Result<(), Box<dyn Error>> {
        let config = parse_str("version: 2\ncommand: [sh, -c, 'exit 0']\n")?;
        let command = ProcessCommand::from_service(
            &config,
            [(OsString::from("PATH"), OsString::from("/bin:/usr/bin"))],
        )?;
        assert!(std::path::Path::new(command.program()).is_absolute());
        assert!(command.program().to_string_lossy().ends_with("/sh"));
        Ok(())
    }

    #[test]
    fn bare_program_without_path_fails_before_broker_start() -> Result<(), Box<dyn Error>> {
        let config = parse_str(
            "version: 2\ncommand: [definitely-not-an-immortal-command]\nenvironment_mode: clear\n",
        )?;
        let error = ProcessCommand::from_service(&config, ProcessEnvironment::new())
            .err()
            .ok_or_else(|| io::Error::other("missing bare program unexpectedly resolved"))?;
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
        Ok(())
    }

    #[test]
    fn invalid_command_fails_before_fork() -> Result<(), Box<dyn Error>> {
        let Err(error) = spawn(ProcessCommand::new(""), Duration::from_secs(1)) else {
            return Err(io::Error::other("an empty executable unexpectedly spawned").into());
        };
        assert_eq!(error.stage(), SpawnStage::Specification);
        assert_eq!(error.failure(), SpawnFailure::OperatingSystem);
        assert!(error.cleanup_pending().is_none());
        Ok(())
    }

    #[test]
    fn every_process_signal_maps_to_a_checked_fork_signal() {
        for signal in [
            ProcessSignal::Hangup,
            ProcessSignal::Interrupt,
            ProcessSignal::Quit,
            ProcessSignal::Kill,
            ProcessSignal::Alarm,
            ProcessSignal::Terminate,
            ProcessSignal::Stop,
            ProcessSignal::Continue,
            ProcessSignal::User1,
            ProcessSignal::User2,
            ProcessSignal::TerminalInput,
            ProcessSignal::TerminalOutput,
            ProcessSignal::WindowChange,
        ] {
            assert!(signal.into_fork().get() > 0);
        }
    }

    #[test]
    fn canonical_fork_events_become_immortal_events() -> Result<(), Box<dyn Error>> {
        let fork_pid =
            fork::ProcessId::new(123).ok_or_else(|| io::Error::other("invalid test process ID"))?;
        let pid = ProcessId::new(123).ok_or_else(|| io::Error::other("invalid test process ID"))?;
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Exited {
                pid: fork_pid,
                code: 42,
            })?,
            ChildEvent::Exited { pid, code: 42 }
        );
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Signalled {
                pid: fork_pid,
                signal: fork::Signal::TERM,
            })?,
            ChildEvent::Signaled {
                pid,
                signal: u8::try_from(fork::Signal::TERM.get())?,
            }
        );
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Stopped {
                pid: fork_pid,
                signal: fork::Signal::STOP,
            })?,
            ChildEvent::Stopped {
                pid,
                signal: u8::try_from(fork::Signal::STOP.get())?,
            }
        );
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Continued { pid: fork_pid })?,
            ChildEvent::Continued { pid }
        );
        Ok(())
    }

    #[test]
    fn process_identifiers_reject_waitpid_selectors() {
        assert!(ProcessId::new(-1).is_none());
        assert!(ProcessId::new(0).is_none());
        assert_eq!(ProcessId::new(1).map(ProcessId::get), Some(1));
    }
}
