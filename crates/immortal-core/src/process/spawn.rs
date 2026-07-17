//! Direct process creation: fork, process-group ownership, and descriptors.
//!
//! `spawn`/`spawn_with_descriptors` prepare and execute one [`ProcessCommand`]
//! inside a brand-new process group through the canonical `fork` crate, and
//! [`spawn_with_descriptors_in_group`] joins an already-reserved group so the
//! broker can guard a spawn with [`ProcessGroupGuard`] before the workload can
//! run unsupervised. Only descriptors named in the explicit allow-list survive
//! `execve`; every other descriptor is closed by the `fork` boundary first.
//! Success means the child completed its startup handshake before the bounded
//! deadline. Any stage that fails after the child exists — a missing process
//! group from `fork`'s contract, a lost handshake — still reports the exact
//! child left for the broker to reap, so a failed spawn never leaks a process.
//!
//! This module is intended to run only inside Immortal's single-threaded
//! process broker; it must not be called from the multi-threaded supervisor
//! runtime.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::time::Duration;

use super::command::{ProcessCommand, SupplementaryGroups};
use super::identity::{ProcessGroupId, ProcessId};

/// Successfully executed direct child and its dedicated generation group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnedProcess {
    process: ProcessId,
    group: ProcessGroupId,
}

#[derive(Debug)]
pub(crate) struct ProcessGroupGuard(fork::ProcessGroupGuard);

impl ProcessGroupGuard {
    pub(crate) fn new() -> io::Result<Self> {
        fork::ProcessGroupGuard::new().map(Self)
    }

    pub(crate) fn group(&self) -> io::Result<ProcessGroupId> {
        ProcessGroupId::new(self.0.process_group().get()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fork returned an invalid guarded process group",
            )
        })
    }

    pub(crate) fn process(&self) -> io::Result<ProcessId> {
        ProcessId::new(self.0.guard_process().get()).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "fork returned an invalid process-group guard",
            )
        })
    }

    pub(crate) fn activate(&mut self, timeout: Duration) -> io::Result<()> {
        self.0.activate(timeout)
    }

    pub(crate) fn disarm(self, timeout: Duration) -> io::Result<()> {
        self.0.disarm(timeout)
    }
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
    pub(super) stage: SpawnStage,
    pub(super) failure: SpawnFailure,
    pub(super) cleanup_pending: Option<ProcessId>,
    pub(super) source: Option<io::Error>,
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
    spawn_prepared(command, startup_timeout, [], fork::ProcessGroup::New)
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
    spawn_prepared(
        command,
        startup_timeout,
        descriptors,
        fork::ProcessGroup::New,
    )
}

pub(super) fn spawn_with_descriptors_in_group(
    command: ProcessCommand,
    startup_timeout: Duration,
    descriptors: impl IntoIterator<Item = ProcessDescriptor>,
    group: ProcessGroupId,
) -> Result<SpawnedProcess, SpawnError> {
    let group = fork::ProcessGroupId::new(group.get()).ok_or(SpawnError {
        stage: SpawnStage::ProcessGroup,
        failure: SpawnFailure::InvalidForkContract,
        cleanup_pending: None,
        source: None,
    })?;
    spawn_prepared(
        command,
        startup_timeout,
        descriptors,
        fork::ProcessGroup::Join(group),
    )
}

fn spawn_prepared(
    command: ProcessCommand,
    startup_timeout: Duration,
    descriptors: impl IntoIterator<Item = ProcessDescriptor>,
    group: fork::ProcessGroup,
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
    prepared.process_group(group);
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

#[cfg(test)]
mod tests {
    use std::io;
    use std::time::Duration;

    use super::{ProcessCommand, SpawnFailure, SpawnStage, spawn};

    #[test]
    fn invalid_command_fails_before_fork() -> Result<(), Box<dyn std::error::Error>> {
        let Err(error) = spawn(ProcessCommand::new(""), Duration::from_secs(1)) else {
            return Err(io::Error::other("an empty executable unexpectedly spawned").into());
        };
        assert_eq!(error.stage(), SpawnStage::Specification);
        assert_eq!(error.failure(), SpawnFailure::OperatingSystem);
        assert!(error.cleanup_pending().is_none());
        Ok(())
    }
}
