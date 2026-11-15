//! Typed, transport-independent supervisor status.

use std::io;

use crate::{
    process::{SpawnFailure, SpawnStage},
    supervisor::{DesiredState, FailureReason, StateMachine, SupervisorState},
};

/// Maximum command arguments accepted in one bounded status frame.
pub const MAX_STATUS_ARGUMENTS: usize = 1024;

/// Lifecycle state without embedding generation-specific process identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceState {
    /// Runtime resources are being initialized.
    Initializing,
    /// No service child exists.
    Down,
    /// Dependencies or a start condition are pending.
    WaitingCondition,
    /// A generation is being created.
    Starting,
    /// The child executed but has not declared readiness.
    Running,
    /// The current generation is ready.
    Ready,
    /// The owned generation is paused by job control.
    Paused,
    /// The owned process group is being stopped.
    Stopping,
    /// A bounded restart delay is pending.
    Backoff,
    /// Post-exit work is running for a completed generation.
    Completed,
    /// Automatic restart stopped after a configured failure.
    Failed,
    /// Supervisor shutdown is in progress.
    Exited,
}

impl ServiceState {
    /// Stable lowercase output name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Initializing => "initializing",
            Self::WaitingCondition => "waiting-condition",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Ready => "ready",
            Self::Paused => "paused",
            Self::Stopping => "stopping",
            Self::Backoff => "backoff",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Exited => "exited",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Down => 1,
            Self::WaitingCondition => 2,
            Self::Starting => 3,
            Self::Running => 4,
            Self::Ready => 5,
            Self::Stopping => 6,
            Self::Backoff => 7,
            Self::Failed => 8,
            Self::Exited => 9,
            Self::Initializing => 10,
            Self::Paused => 11,
            Self::Completed => 12,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Down),
            2 => Some(Self::WaitingCondition),
            3 => Some(Self::Starting),
            4 => Some(Self::Running),
            5 => Some(Self::Ready),
            6 => Some(Self::Stopping),
            7 => Some(Self::Backoff),
            8 => Some(Self::Failed),
            9 => Some(Self::Exited),
            10 => Some(Self::Initializing),
            11 => Some(Self::Paused),
            12 => Some(Self::Completed),
            _ => None,
        }
    }
}

/// Readiness of the current generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessStatus {
    /// No generation is currently awaiting readiness.
    NotApplicable,
    /// The child executed and the readiness deadline is active.
    Waiting,
    /// The current generation declared readiness.
    Ready,
    /// The most recent readiness deadline elapsed.
    TimedOut,
}

impl ReadinessStatus {
    /// Stable lowercase output name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotApplicable => "n/a",
            Self::Waiting => "waiting",
            Self::Ready => "ready",
            Self::TimedOut => "timed-out",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::NotApplicable => 0,
            Self::Waiting => 1,
            Self::Ready => 2,
            Self::TimedOut => 3,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::NotApplicable),
            1 => Some(Self::Waiting),
            2 => Some(Self::Ready),
            3 => Some(Self::TimedOut),
            _ => None,
        }
    }
}

/// Aggregate health of all configured logger pipelines.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggerStatus {
    /// No logger pipeline is configured or status is not yet available.
    NotConfigured,
    /// At least one logger stage is starting.
    Starting,
    /// Every logger stage is ready.
    Ready,
    /// At least one logger is in restart backoff.
    Backoff,
    /// At least one logger exhausted its restart policy.
    Failed,
}

impl LoggerStatus {
    /// Stable lowercase output name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotConfigured => "n/a",
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Backoff => "backoff",
            Self::Failed => "failed",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::NotConfigured => 0,
            Self::Starting => 1,
            Self::Ready => 2,
            Self::Backoff => 3,
            Self::Failed => 4,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::NotConfigured),
            1 => Some(Self::Starting),
            2 => Some(Self::Ready),
            3 => Some(Self::Backoff),
            4 => Some(Self::Failed),
            _ => None,
        }
    }
}

/// Last terminal generation outcome retained for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LastResult {
    /// Main child exited with a status code.
    Exited(u8),
    /// Main child was terminated by a signal.
    Signaled(u8),
    /// Child creation or exec failed.
    SpawnFailed,
    /// Child failed to become ready before its deadline.
    ReadinessTimeout,
    /// A descriptor-tracked generation ended when its lifetime capability closed.
    LifetimeClosed,
    /// A descriptor-tracked generation violated its lifetime capability contract.
    LifetimeFailed,
}

impl LastResult {
    /// Stable display value.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Exited(code) => format!("exit:{code}"),
            Self::Signaled(signal) => format!("signal:{signal}"),
            Self::SpawnFailed => "spawn-failed".to_owned(),
            Self::ReadinessTimeout => "readiness-timeout".to_owned(),
            Self::LifetimeClosed => "lifetime-closed".to_owned(),
            Self::LifetimeFailed => "lifetime-failed".to_owned(),
        }
    }

    pub(crate) const fn code(self) -> (u8, u8) {
        match self {
            Self::Exited(value) => (1, value),
            Self::Signaled(value) => (2, value),
            Self::SpawnFailed => (3, 0),
            Self::ReadinessTimeout => (4, 0),
            Self::LifetimeClosed => (5, 0),
            Self::LifetimeFailed => (6, 0),
        }
    }

    pub(crate) const fn from_code(kind: u8, value: u8) -> Option<Self> {
        match kind {
            1 => Some(Self::Exited(value)),
            2 => Some(Self::Signaled(value)),
            3 if value == 0 => Some(Self::SpawnFailed),
            4 if value == 0 => Some(Self::ReadinessTimeout),
            5 if value == 0 => Some(Self::LifetimeClosed),
            6 if value == 0 => Some(Self::LifetimeFailed),
            _ => None,
        }
    }
}

/// Why a process could not be created: the stage that failed and its cause.
///
/// Status alone must answer why a service or logger is not running, because a
/// daemonized supervisor has nowhere else to report it. The value is three
/// fixed fields, so it needs no truncation or escaping.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SpawnDetail {
    /// Stage of process creation that failed.
    pub stage: SpawnStage,
    /// Classification of the failure.
    pub failure: SpawnFailure,
    /// Operating-system error number, when the failure carried one.
    pub os_error: Option<i32>,
}

impl SpawnDetail {
    /// Stable machine-readable value, such as `execute:os-error:2`.
    #[must_use]
    pub fn name(self) -> String {
        match self.os_error {
            Some(error) => format!("{}:{}:{error}", self.stage.name(), self.failure.name()),
            None => format!("{}:{}", self.stage.name(), self.failure.name()),
        }
    }

    /// Human-readable sentence fragment, such as
    /// `execution failed: No such file or directory (os error 2)`.
    #[must_use]
    pub fn describe(self) -> String {
        match (self.failure, self.os_error) {
            (SpawnFailure::OperatingSystem, Some(error)) => {
                format!(
                    "{} failed: {}",
                    self.stage,
                    io::Error::from_raw_os_error(error)
                )
            }
            (SpawnFailure::OperatingSystem, None) => format!("{} failed", self.stage),
            (SpawnFailure::TimedOut, _) => format!("{} timed out", self.stage),
            (SpawnFailure::InvalidHandshake, _) => {
                format!("{} returned an invalid record", self.stage)
            }
            (SpawnFailure::InvalidForkContract, _) => {
                format!("{} violated the fork contract", self.stage)
            }
        }
    }
}

/// Most recent failure of a logger stage, retained until loggers are healthy.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggerLastResult {
    /// The logger process could not be created.
    SpawnFailed(SpawnDetail),
    /// The logger exited with a status code.
    Exited(u8),
    /// The logger was terminated by a signal.
    Signaled(u8),
}

impl LoggerLastResult {
    /// Stable machine-readable value.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::SpawnFailed(detail) => format!("spawn-failed:{}", detail.name()),
            Self::Exited(code) => format!("exit:{code}"),
            Self::Signaled(signal) => format!("signal:{signal}"),
        }
    }

    /// Human-readable sentence fragment naming the logger's failure.
    #[must_use]
    pub fn describe(self) -> String {
        match self {
            Self::SpawnFailed(detail) => detail.describe(),
            Self::Exited(code) => format!("exited with status {code}"),
            Self::Signaled(signal) => format!("terminated by signal {signal}"),
        }
    }
}

/// Complete bounded status published by one supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSnapshot {
    /// Supervisor process ID after the status-serving runtime has started.
    pub supervisor_pid: Option<u32>,
    /// Owned main-child PID, never an adopted PID-file value.
    pub main_pid: Option<u32>,
    /// Persistent operator intent.
    pub desired: DesiredState,
    /// Current lifecycle state.
    pub state: ServiceState,
    /// Current readiness state.
    pub readiness: ReadinessStatus,
    /// Seconds the current generation has been alive.
    pub uptime_seconds: Option<u64>,
    /// Seconds since the most recent generation ended.
    pub down_seconds: Option<u64>,
    /// Number of generation start attempts.
    pub starts: u64,
    /// Number of unsuccessful terminal outcomes.
    pub failures: u64,
    /// Most recent terminal result.
    pub last_result: Option<LastResult>,
    /// Stage and cause when `last_result` is [`LastResult::SpawnFailed`].
    pub last_spawn_failure: Option<SpawnDetail>,
    /// Remaining restart delay in seconds.
    pub backoff_seconds: Option<u64>,
    /// Aggregate logging pipeline health.
    pub logger: LoggerStatus,
    /// Most recent logger failure while a logger is not healthy.
    pub logger_last_result: Option<LoggerLastResult>,
    /// Exact configured service argv.
    pub command: Vec<String>,
}

impl StatusSnapshot {
    /// Produce a valid childless snapshot before the process executor exists.
    #[must_use]
    pub fn from_machine(machine: &StateMachine) -> Self {
        let state = match machine.state() {
            SupervisorState::Initializing => ServiceState::Initializing,
            SupervisorState::Down => ServiceState::Down,
            SupervisorState::WaitingCondition => ServiceState::WaitingCondition,
            SupervisorState::Starting(_) => ServiceState::Starting,
            SupervisorState::Running(_) => ServiceState::Running,
            SupervisorState::Ready(_) => ServiceState::Ready,
            SupervisorState::Paused { .. } => ServiceState::Paused,
            SupervisorState::Stopping(_) => ServiceState::Stopping,
            SupervisorState::Backoff { .. } => ServiceState::Backoff,
            SupervisorState::Completed(_) => ServiceState::Completed,
            SupervisorState::Failed(_) => ServiceState::Failed,
            SupervisorState::Exited => ServiceState::Exited,
        };
        let readiness = match machine.state() {
            SupervisorState::Running(_) | SupervisorState::Paused { ready: false, .. } => {
                ReadinessStatus::Waiting
            }
            SupervisorState::Ready(_) | SupervisorState::Paused { ready: true, .. } => {
                ReadinessStatus::Ready
            }
            SupervisorState::Failed(FailureReason::ReadinessTimeout) => ReadinessStatus::TimedOut,
            _ => ReadinessStatus::NotApplicable,
        };
        let backoff_seconds = match machine.state() {
            SupervisorState::Backoff { delay_seconds, .. } => Some(delay_seconds),
            _ => None,
        };
        Self {
            supervisor_pid: None,
            main_pid: None,
            desired: machine.desired(),
            state,
            readiness,
            uptime_seconds: None,
            down_seconds: None,
            starts: 0,
            failures: 0,
            last_result: None,
            last_spawn_failure: None,
            backoff_seconds,
            logger: LoggerStatus::NotConfigured,
            logger_last_result: None,
            command: Vec::new(),
        }
    }
}

/// Stable desired-state output name.
#[must_use]
pub const fn desired_state_name(desired: DesiredState) -> &'static str {
    match desired {
        DesiredState::Up => "up",
        DesiredState::Down => "down",
        DesiredState::Once => "once",
        DesiredState::Halt => "halt",
        DesiredState::Exit => "exit",
    }
}

pub(crate) const fn desired_state_code(desired: DesiredState) -> u8 {
    match desired {
        DesiredState::Up => 1,
        DesiredState::Down => 2,
        DesiredState::Once => 3,
        DesiredState::Halt => 4,
        DesiredState::Exit => 5,
    }
}

pub(crate) const fn desired_state_from_code(code: u8) -> Option<DesiredState> {
    match code {
        1 => Some(DesiredState::Up),
        2 => Some(DesiredState::Down),
        3 => Some(DesiredState::Once),
        4 => Some(DesiredState::Halt),
        5 => Some(DesiredState::Exit),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{LastResult, LoggerLastResult, ServiceState, SpawnDetail};
    use crate::process::{SpawnFailure, SpawnStage};

    /// The machine value is stable and the sentence names stage and cause.
    #[test]
    fn spawn_detail_names_the_stage_and_cause() {
        let missing = SpawnDetail {
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
        };
        assert_eq!(missing.name(), "execute:os-error:2");
        assert!(missing.describe().starts_with("execution failed: "));
        assert!(missing.describe().contains("os error 2"));

        let slow = SpawnDetail {
            stage: SpawnStage::StartupHandshake,
            failure: SpawnFailure::TimedOut,
            os_error: None,
        };
        assert_eq!(slow.name(), "handshake:timed-out");
        assert_eq!(slow.describe(), "startup handshake timed out");

        assert_eq!(
            LoggerLastResult::SpawnFailed(missing).name(),
            "spawn-failed:execute:os-error:2"
        );
        assert_eq!(LoggerLastResult::Exited(3).name(), "exit:3");
        assert_eq!(LoggerLastResult::Signaled(9).name(), "signal:9");
        assert_eq!(
            LoggerLastResult::Exited(3).describe(),
            "exited with status 3"
        );
    }

    /// Stage and classification codes are shared with the broker protocol, so
    /// each must survive a round trip and reject unknown values.
    #[test]
    fn spawn_codes_round_trip_and_reject_unknown_values() {
        for stage in [
            SpawnStage::Specification,
            SpawnStage::Fork,
            SpawnStage::ProcessGroup,
            SpawnStage::CurrentDirectory,
            SpawnStage::DescriptorMapping,
            SpawnStage::Execute,
            SpawnStage::StartupHandshake,
            SpawnStage::Identity,
            SpawnStage::SignalState,
        ] {
            assert_eq!(SpawnStage::from_code(stage.code()), Some(stage));
        }
        for failure in [
            SpawnFailure::OperatingSystem,
            SpawnFailure::TimedOut,
            SpawnFailure::InvalidHandshake,
            SpawnFailure::InvalidForkContract,
        ] {
            assert_eq!(SpawnFailure::from_code(failure.code()), Some(failure));
        }
        assert_eq!(SpawnStage::from_code(0), None);
        assert_eq!(SpawnStage::from_code(10), None);
        assert_eq!(SpawnFailure::from_code(0), None);
        assert_eq!(SpawnFailure::from_code(5), None);
    }

    #[test]
    fn service_state_codes_are_unique_and_round_trip() {
        let states = [
            ServiceState::Initializing,
            ServiceState::Down,
            ServiceState::WaitingCondition,
            ServiceState::Starting,
            ServiceState::Running,
            ServiceState::Ready,
            ServiceState::Paused,
            ServiceState::Stopping,
            ServiceState::Backoff,
            ServiceState::Completed,
            ServiceState::Failed,
            ServiceState::Exited,
        ];
        let mut codes = states.map(ServiceState::code);
        codes.sort_unstable();
        assert_eq!(codes, [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);

        for state in states {
            assert_eq!(ServiceState::from_code(state.code()), Some(state));
        }
        assert_eq!(ServiceState::from_code(0), None);
        assert_eq!(ServiceState::from_code(13), None);
    }

    #[test]
    fn last_result_codes_round_trip_and_reject_invalid_payloads() {
        for result in [
            LastResult::Exited(7),
            LastResult::Signaled(9),
            LastResult::SpawnFailed,
            LastResult::ReadinessTimeout,
            LastResult::LifetimeClosed,
            LastResult::LifetimeFailed,
        ] {
            let (kind, value) = result.code();
            assert_eq!(LastResult::from_code(kind, value), Some(result));
        }
        assert_eq!(LastResult::from_code(5, 1), None);
        assert_eq!(LastResult::from_code(6, 1), None);
        assert_eq!(LastResult::from_code(7, 0), None);
    }
}
