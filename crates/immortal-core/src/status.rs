//! Typed, transport-independent supervisor status.

use crate::supervisor::{DesiredState, FailureReason, StateMachine, SupervisorState};

/// Maximum command arguments accepted in one bounded status frame.
pub const MAX_STATUS_ARGUMENTS: usize = 1024;

/// Lifecycle state without embedding generation-specific process identity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServiceState {
    /// No service child exists.
    Down,
    /// Dependencies or a start condition are pending.
    Waiting,
    /// A generation is being created.
    Starting,
    /// The child executed but has not declared readiness.
    Started,
    /// The current generation is ready.
    Ready,
    /// The owned process group is being stopped.
    Stopping,
    /// A bounded restart delay is pending.
    Backoff,
    /// Automatic restart stopped after a configured failure.
    Failed,
    /// Supervisor shutdown is in progress.
    Exiting,
}

impl ServiceState {
    /// Stable lowercase output name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Down => "down",
            Self::Waiting => "waiting",
            Self::Starting => "starting",
            Self::Started => "started",
            Self::Ready => "ready",
            Self::Stopping => "stopping",
            Self::Backoff => "backoff",
            Self::Failed => "failed",
            Self::Exiting => "exiting",
        }
    }

    pub(crate) const fn code(self) -> u8 {
        match self {
            Self::Down => 1,
            Self::Waiting => 2,
            Self::Starting => 3,
            Self::Started => 4,
            Self::Ready => 5,
            Self::Stopping => 6,
            Self::Backoff => 7,
            Self::Failed => 8,
            Self::Exiting => 9,
        }
    }

    pub(crate) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Down),
            2 => Some(Self::Waiting),
            3 => Some(Self::Starting),
            4 => Some(Self::Started),
            5 => Some(Self::Ready),
            6 => Some(Self::Stopping),
            7 => Some(Self::Backoff),
            8 => Some(Self::Failed),
            9 => Some(Self::Exiting),
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
        }
    }

    pub(crate) const fn code(self) -> (u8, u8) {
        match self {
            Self::Exited(value) => (1, value),
            Self::Signaled(value) => (2, value),
            Self::SpawnFailed => (3, 0),
            Self::ReadinessTimeout => (4, 0),
        }
    }

    pub(crate) const fn from_code(kind: u8, value: u8) -> Option<Self> {
        match kind {
            1 => Some(Self::Exited(value)),
            2 => Some(Self::Signaled(value)),
            3 if value == 0 => Some(Self::SpawnFailed),
            4 if value == 0 => Some(Self::ReadinessTimeout),
            _ => None,
        }
    }
}

/// Complete bounded status published by one supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StatusSnapshot {
    /// Supervisor process ID when runtime initialization has completed.
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
    /// Remaining restart delay in seconds.
    pub backoff_seconds: Option<u64>,
    /// Aggregate logging pipeline health.
    pub logger: LoggerStatus,
    /// Exact configured service argv.
    pub command: Vec<String>,
}

impl StatusSnapshot {
    /// Produce a valid childless snapshot before the process executor exists.
    #[must_use]
    pub fn from_machine(machine: &StateMachine) -> Self {
        let state = match machine.state() {
            SupervisorState::Down => ServiceState::Down,
            SupervisorState::Waiting => ServiceState::Waiting,
            SupervisorState::Starting(_) => ServiceState::Starting,
            SupervisorState::Started(_) => ServiceState::Started,
            SupervisorState::Ready(_) => ServiceState::Ready,
            SupervisorState::Stopping(_) => ServiceState::Stopping,
            SupervisorState::Backoff { .. } => ServiceState::Backoff,
            SupervisorState::Failed(_) => ServiceState::Failed,
            SupervisorState::Exiting => ServiceState::Exiting,
        };
        let readiness = match machine.state() {
            SupervisorState::Started(_) => ReadinessStatus::Waiting,
            SupervisorState::Ready(_) => ReadinessStatus::Ready,
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
            backoff_seconds,
            logger: LoggerStatus::NotConfigured,
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
