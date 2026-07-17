//! Deterministic process-supervision state and restart policy.
//!
//! The supervision model is assembled here from focused children: `generation`
//! owns child identity, `state` owns the state vocabulary and transition
//! errors, `machine` owns the deterministic state machine, and `restart` owns
//! the restart decision and rate-limit policy. The public surface is unchanged
//! and no process I/O lives in this module; that responsibility belongs to the
//! executor.

mod generation;
mod machine;
mod restart;
mod state;

#[cfg(test)]
mod tests;

pub use self::{
    generation::Generation,
    machine::StateMachine,
    restart::{ConditionRetry, ConditionTracker, RestartDecision, RestartTracker},
    state::{ChildResult, DesiredState, FailureReason, SupervisorState, TransitionError},
};
