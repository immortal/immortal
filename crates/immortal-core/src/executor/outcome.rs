//! Public executor result types.
//!
//! These small value types are the stable observation boundary for foreground
//! and daemon execution. They remain in a private child so the facade can keep
//! `immortal_core::executor::*` as the only reachable public path.

use super::{ChildResult, FailureReason, SupervisorState};

/// Final observation returned by the foreground executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupervisionOutcome {
    /// Terminal supervisor state reached by this foreground invocation.
    pub state: SupervisorState,
    /// Restart-limit failure which requested terminal cleanup instead of persistent failure.
    pub terminal_failure: Option<FailureReason>,
    /// Last reaped generation result, absent when shutdown preceded the first start.
    pub last_result: Option<ChildResult>,
    /// Whether the last recorded result represents a failed exec handshake.
    pub last_start_failed: bool,
    /// Whether the last generation failed its descriptor readiness contract.
    pub last_readiness_failed: bool,
    /// Number of generation attempts, including failed exec handshakes.
    pub starts: u64,
}

/// Result observed by a process which participates in checked daemon startup.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DaemonRunOutcome {
    /// The original invoking process may return success to its caller.
    Parent,
    /// The detached supervisor eventually completed its lifecycle.
    Daemon(SupervisionOutcome),
}
