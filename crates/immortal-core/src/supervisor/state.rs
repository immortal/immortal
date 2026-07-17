//! Supervision state vocabulary and transition errors.
//!
//! These types model operator intent, child results, failure reasons, the
//! externally observable supervisor state, and the errors returned when an
//! event is invalid for the current state. They carry no process I/O and form
//! the alphabet the state machine and restart policy operate on.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

use crate::config::RestartConfig;

use super::Generation;

/// Persistent operator intent for the service.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DesiredState {
    /// Keep starting generations according to restart policy.
    #[default]
    Up,
    /// Keep the supervisor alive without a child.
    Down,
    /// Run exactly one generation, then remain down.
    Once,
    /// Stop the child and terminate the supervisor.
    Halt,
    /// Terminate the supervisor while deliberately leaving its child running.
    Exit,
}

/// Result obtained by reaping the main child.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildResult {
    /// Child called `_exit` or returned from its main function.
    Exited(u8),
    /// Child was terminated by a signal.
    Signaled(u8),
    /// Every inherited lifetime descriptor closed without protocol misuse.
    LifetimeClosed,
    /// The lifetime descriptor was used as data rather than held as a capability.
    LifetimeFailed,
}

impl ChildResult {
    /// Whether this result matches the configured successful exit codes.
    #[must_use]
    pub fn is_success(self, restart: &RestartConfig) -> bool {
        matches!(self, Self::Exited(code) if restart.success_exit_codes.contains(&code))
    }
}

/// Stable reason for entering the `Failed` state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FailureReason {
    /// Configured retry count was exhausted.
    RetryLimit,
    /// Configured total supervision duration was exhausted.
    ElapsedTimeLimit,
    /// Too many starts occurred inside the configured rolling window.
    BurstLimit,
    /// Child did not signal readiness before its deadline.
    ReadinessTimeout,
    /// Supervisor could not create or execute a child.
    SpawnFailed,
    /// A required logger stage exhausted its independent retry policy.
    LoggerRetryLimit,
}

/// Externally observable supervisor lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorState {
    /// Runtime resources are being initialized before supervision begins.
    Initializing,
    /// No child exists and no immediate start is scheduled.
    Down,
    /// Preconditions are being evaluated.
    WaitingCondition,
    /// A generation is being created and executed.
    Starting(Generation),
    /// A child exists but has not yet become ready.
    Running(Generation),
    /// The generation is ready and running.
    Ready(Generation),
    /// The generation is stopped by a job-control signal.
    Paused {
        /// Exact generation which remains owned.
        generation: Generation,
        /// Whether readiness had completed before the stop.
        ready: bool,
    },
    /// Lifecycle shutdown is in progress.
    Stopping(Generation),
    /// A restart is scheduled after the given base delay.
    Backoff {
        /// Generation which most recently completed.
        generation: Generation,
        /// Delay before attempting the next generation.
        delay_seconds: u64,
    },
    /// The service ended and bounded post-exit work is running.
    Completed(Generation),
    /// Automatic starts are disabled until an operator requests `Up`.
    Failed(FailureReason),
    /// Supervisor should finish after cleanup and status publication.
    Exited,
}

impl SupervisorState {
    /// Return the generation associated with a live or recently completed child.
    #[must_use]
    pub const fn generation(self) -> Option<Generation> {
        match self {
            Self::Starting(generation)
            | Self::Running(generation)
            | Self::Ready(generation)
            | Self::Stopping(generation)
            | Self::Paused { generation, .. }
            | Self::Completed(generation)
            | Self::Backoff { generation, .. } => Some(generation),
            Self::Initializing
            | Self::Down
            | Self::WaitingCondition
            | Self::Failed(_)
            | Self::Exited => None,
        }
    }

    /// Return the generation only when an owned child may still be alive.
    #[must_use]
    pub const fn live_generation(self) -> Option<Generation> {
        match self {
            Self::Starting(generation)
            | Self::Running(generation)
            | Self::Ready(generation)
            | Self::Stopping(generation)
            | Self::Paused { generation, .. } => Some(generation),
            Self::Initializing
            | Self::Down
            | Self::WaitingCondition
            | Self::Backoff { .. }
            | Self::Completed(_)
            | Self::Failed(_)
            | Self::Exited => None,
        }
    }
}

/// Invalid lifecycle event or exhausted generation space.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransitionError {
    /// Event is not valid in the current lifecycle state.
    Invalid {
        /// State in which the event was attempted.
        state: SupervisorState,
        /// Event which was rejected.
        event: &'static str,
    },
    /// The 64-bit generation counter cannot be incremented.
    GenerationExhausted,
}

impl Display for TransitionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { state, event } => {
                write!(formatter, "event `{event}` is invalid in state {state:?}")
            }
            Self::GenerationExhausted => formatter.write_str("service generation exhausted"),
        }
    }
}

impl Error for TransitionError {}
