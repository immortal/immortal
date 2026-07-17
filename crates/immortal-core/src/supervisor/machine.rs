//! Deterministic supervision state machine.
//!
//! [`StateMachine`] is the pure lifecycle model: it validates every generation
//! transition, rejects stale or out-of-order events without mutation, and maps
//! a reaped child's restart decision onto the next observable state. All
//! process I/O is performed by a separate executor.

use super::{
    DesiredState, FailureReason, Generation, RestartDecision, SupervisorState, TransitionError,
};

/// Pure lifecycle model. Process I/O is performed by a separate executor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StateMachine {
    desired: DesiredState,
    state: SupervisorState,
    next_generation: Generation,
}

impl Default for StateMachine {
    fn default() -> Self {
        Self {
            desired: DesiredState::Up,
            state: SupervisorState::Down,
            next_generation: Generation::FIRST,
        }
    }
}

impl StateMachine {
    /// Construct a lifecycle before runtime initialization completes.
    #[must_use]
    pub const fn initializing(desired: DesiredState) -> Self {
        Self {
            desired,
            state: SupervisorState::Initializing,
            next_generation: Generation::FIRST,
        }
    }

    /// Publish successful runtime initialization.
    ///
    /// # Errors
    ///
    /// Returns an error unless the lifecycle is initializing.
    pub fn initialized(&mut self) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Initializing {
            return Err(self.invalid("initialized"));
        }
        self.state = SupervisorState::Down;
        Ok(())
    }

    /// Construct a lifecycle with explicit initial operator intent.
    #[must_use]
    pub const fn new(desired: DesiredState) -> Self {
        Self {
            desired,
            state: SupervisorState::Down,
            next_generation: Generation::FIRST,
        }
    }

    /// Current operator intent.
    #[must_use]
    pub const fn desired(&self) -> DesiredState {
        self.desired
    }

    /// Current observable lifecycle state.
    #[must_use]
    pub const fn state(&self) -> SupervisorState {
        self.state
    }

    /// Set operator intent without implicitly pretending a lifecycle operation completed.
    pub const fn set_desired(&mut self, desired: DesiredState) {
        self.desired = desired;
    }

    /// Begin evaluation of dependencies and start conditions.
    ///
    /// # Errors
    ///
    /// Returns an error unless no child exists and the desired state permits a start.
    pub fn begin_start(&mut self) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Down
            || !matches!(self.desired, DesiredState::Up | DesiredState::Once)
        {
            return Err(self.invalid("begin_start"));
        }
        self.state = SupervisorState::WaitingCondition;
        Ok(())
    }

    /// Record successful preconditions and allocate the next generation.
    ///
    /// # Errors
    ///
    /// Returns an error unless the lifecycle is waiting, or generation identity is exhausted.
    pub fn preconditions_ready(&mut self) -> Result<Generation, TransitionError> {
        if self.state != SupervisorState::WaitingCondition {
            return Err(self.invalid("preconditions_ready"));
        }
        let generation = self.next_generation;
        self.next_generation = generation.next()?;
        self.state = SupervisorState::Starting(generation);
        Ok(generation)
    }

    /// Record a successfully executed child which is awaiting readiness.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or invalid current state.
    pub fn child_started(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Starting(generation) {
            return Err(self.invalid("child_started"));
        }
        self.state = SupervisorState::Running(generation);
        Ok(())
    }

    /// Record readiness for the current generation.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or invalid current state.
    pub fn child_ready(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Running(generation) {
            return Err(self.invalid("child_ready"));
        }
        self.state = SupervisorState::Ready(generation);
        Ok(())
    }

    /// Record a job-control stop while retaining exact generation ownership.
    ///
    /// # Errors
    ///
    /// Returns an error unless the current running or ready generation matches.
    pub fn child_paused(&mut self, generation: Generation) -> Result<(), TransitionError> {
        let ready = match self.state {
            SupervisorState::Running(current) if current == generation => false,
            SupervisorState::Ready(current) if current == generation => true,
            _ => return Err(self.invalid("child_paused")),
        };
        self.state = SupervisorState::Paused { generation, ready };
        Ok(())
    }

    /// Record continuation of an exactly owned paused generation.
    ///
    /// # Errors
    ///
    /// Returns an error unless the paused generation matches.
    pub fn child_continued(&mut self, generation: Generation) -> Result<(), TransitionError> {
        let SupervisorState::Paused {
            generation: current,
            ready,
        } = self.state
        else {
            return Err(self.invalid("child_continued"));
        };
        if current != generation {
            return Err(self.invalid("child_continued"));
        }
        self.state = if ready {
            SupervisorState::Ready(generation)
        } else {
            SupervisorState::Running(generation)
        };
        Ok(())
    }

    /// Begin deterministic lifecycle shutdown of the owned process group.
    ///
    /// # Errors
    ///
    /// Returns an error when there is no current child or the generation is stale.
    pub fn begin_stop(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if !matches!(
            self.state,
            SupervisorState::Starting(current)
                | SupervisorState::Running(current)
                | SupervisorState::Ready(current)
                | SupervisorState::Paused { generation: current, .. }
                if current == generation
        ) {
            return Err(self.invalid("begin_stop"));
        }
        self.state = SupervisorState::Stopping(generation);
        Ok(())
    }

    /// Abort a descriptor-hook stop which did not establish service shutdown.
    ///
    /// The logical generation remains owned even when its original launcher
    /// has exited; callers restore whether the generation had reached ready.
    ///
    /// # Errors
    ///
    /// Returns an error unless the exact generation is currently stopping.
    pub fn abort_stop(
        &mut self,
        generation: Generation,
        ready: bool,
        desired: DesiredState,
    ) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Stopping(generation) {
            return Err(self.invalid("abort_stop"));
        }
        self.desired = desired;
        self.state = if ready {
            SupervisorState::Ready(generation)
        } else {
            SupervisorState::Running(generation)
        };
        Ok(())
    }

    /// Record deliberate broker detachment of a live generation for `Exit`.
    ///
    /// # Errors
    ///
    /// Returns an error unless the exact generation is live and operator intent
    /// requests supervisor exit without stopping the child.
    pub fn child_detached(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if self.desired != DesiredState::Exit
            || !matches!(
                self.state,
                SupervisorState::Starting(current)
                    | SupervisorState::Running(current)
                    | SupervisorState::Ready(current)
                    | SupervisorState::Paused { generation: current, .. }
                    if current == generation
            )
        {
            return Err(self.invalid("child_detached"));
        }
        self.state = SupervisorState::Exited;
        Ok(())
    }

    /// Publish the decision made after reaping the current child.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or a state with no reapable child.
    pub fn child_reaped(
        &mut self,
        generation: Generation,
        decision: RestartDecision,
    ) -> Result<(), TransitionError> {
        if !matches!(
            self.state,
            SupervisorState::Starting(current)
                | SupervisorState::Running(current)
                | SupervisorState::Ready(current)
                | SupervisorState::Stopping(current)
                | SupervisorState::Paused { generation: current, .. }
                if current == generation
        ) {
            return Err(self.invalid("child_reaped"));
        }
        self.state = completion_state(generation, decision);
        Ok(())
    }

    /// Retain a terminal generation while bounded post-exit work runs.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or a state with no reapable child.
    pub fn child_completed(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if self.state.live_generation() != Some(generation) {
            return Err(self.invalid("child_completed"));
        }
        self.state = SupervisorState::Completed(generation);
        Ok(())
    }

    /// Finish post-exit work and publish the already selected restart decision.
    ///
    /// # Errors
    ///
    /// Returns an error unless the exact completed generation is current.
    pub fn completion_finished(
        &mut self,
        generation: Generation,
        decision: RestartDecision,
    ) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Completed(generation) {
            return Err(self.invalid("completion_finished"));
        }
        self.state = completion_state(generation, decision);
        Ok(())
    }

    /// Finish a scheduled backoff and permit another start.
    ///
    /// # Errors
    ///
    /// Returns an error unless the supplied generation owns the current backoff.
    pub fn backoff_elapsed(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if !matches!(
            self.state,
            SupervisorState::Backoff {
                generation: current,
                ..
            } if current == generation
        ) {
            return Err(self.invalid("backoff_elapsed"));
        }
        self.state = if matches!(self.desired, DesiredState::Up | DesiredState::Once) {
            SupervisorState::Down
        } else if matches!(self.desired, DesiredState::Halt | DesiredState::Exit) {
            SupervisorState::Exited
        } else {
            SupervisorState::Down
        };
        Ok(())
    }

    /// Cancel pending condition/start-delay or restart-backoff work.
    ///
    /// # Errors
    ///
    /// Returns an error unless the lifecycle is waiting without a child.
    pub fn cancel_pending(&mut self) -> Result<(), TransitionError> {
        if !matches!(
            self.state,
            SupervisorState::WaitingCondition | SupervisorState::Backoff { .. }
        ) {
            return Err(self.invalid("cancel_pending"));
        }
        self.state = if matches!(self.desired, DesiredState::Halt | DesiredState::Exit) {
            SupervisorState::Exited
        } else {
            SupervisorState::Down
        };
        Ok(())
    }

    /// Exit from a childless stable state after Halt or Exit intent.
    ///
    /// # Errors
    ///
    /// Returns an error when a child may be alive or operator intent does not request exit.
    pub fn exit_without_child(&mut self) -> Result<(), TransitionError> {
        if !matches!(
            self.state,
            SupervisorState::Down | SupervisorState::Failed(_)
        ) || !matches!(self.desired, DesiredState::Halt | DesiredState::Exit)
        {
            return Err(self.invalid("exit_without_child"));
        }
        self.state = SupervisorState::Exited;
        Ok(())
    }

    /// Clear configured failure after an explicit operator `Up` request.
    ///
    /// # Errors
    ///
    /// Returns an error unless currently failed.
    pub fn reset_failure(&mut self) -> Result<(), TransitionError> {
        if !matches!(self.state, SupervisorState::Failed(_)) {
            return Err(self.invalid("reset_failure"));
        }
        self.desired = DesiredState::Up;
        self.state = SupervisorState::Down;
        Ok(())
    }

    /// Enter a configured failure before any service generation exists.
    ///
    /// # Errors
    ///
    /// Returns an error unless the supervisor is childless and stable.
    pub fn fail_without_child(&mut self, reason: FailureReason) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Down {
            return Err(self.invalid("fail_without_child"));
        }
        self.state = SupervisorState::Failed(reason);
        Ok(())
    }

    const fn invalid(&self, event: &'static str) -> TransitionError {
        TransitionError::Invalid {
            state: self.state,
            event,
        }
    }
}
const fn completion_state(generation: Generation, decision: RestartDecision) -> SupervisorState {
    match decision {
        RestartDecision::Restart {
            base_delay_seconds, ..
        } => SupervisorState::Backoff {
            generation,
            delay_seconds: base_delay_seconds,
        },
        RestartDecision::StayDown => SupervisorState::Down,
        RestartDecision::ExitSupervisor | RestartDecision::ExitFailure(_) => {
            SupervisorState::Exited
        }
        RestartDecision::Fail(reason) => SupervisorState::Failed(reason),
    }
}
