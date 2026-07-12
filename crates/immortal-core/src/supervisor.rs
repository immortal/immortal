//! Deterministic process-supervision state and restart policy.

use std::{
    collections::VecDeque,
    error::Error,
    fmt::{self, Display, Formatter},
};

use crate::config::{RestartConfig, RestartPolicy, StartConditionConfig};

/// Monotonic identity assigned to each child generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(u64);

impl Generation {
    /// First valid child generation.
    pub const FIRST: Self = Self(1);

    /// Construct a generation from a nonzero persisted or test value.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Return the numeric value used for status and protocol messages.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Reconstruct a nonzero generation received from the validated control protocol.
    #[must_use]
    pub(crate) const fn from_protocol(value: u64) -> Self {
        Self(value)
    }

    fn next(self) -> Result<Self, TransitionError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(TransitionError::GenerationExhausted)
    }
}

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
}

/// Externally observable supervisor lifecycle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorState {
    /// No child exists and no immediate start is scheduled.
    Down,
    /// Preconditions are being evaluated.
    Waiting,
    /// A generation is being created and executed.
    Starting(Generation),
    /// A child exists but has not yet become ready.
    Started(Generation),
    /// The generation is ready and running.
    Ready(Generation),
    /// Lifecycle shutdown is in progress.
    Stopping(Generation),
    /// A restart is scheduled after the given base delay.
    Backoff {
        /// Generation which most recently completed.
        generation: Generation,
        /// Delay before attempting the next generation.
        delay_seconds: u64,
    },
    /// Automatic starts are disabled until an operator requests `Up`.
    Failed(FailureReason),
    /// Supervisor should finish after cleanup and status publication.
    Exiting,
}

impl SupervisorState {
    /// Return the generation associated with a live or recently completed child.
    #[must_use]
    pub const fn generation(self) -> Option<Generation> {
        match self {
            Self::Starting(generation)
            | Self::Started(generation)
            | Self::Ready(generation)
            | Self::Stopping(generation)
            | Self::Backoff { generation, .. } => Some(generation),
            Self::Down | Self::Waiting | Self::Failed(_) | Self::Exiting => None,
        }
    }

    /// Return the generation only when an owned child may still be alive.
    #[must_use]
    pub const fn live_generation(self) -> Option<Generation> {
        match self {
            Self::Starting(generation)
            | Self::Started(generation)
            | Self::Ready(generation)
            | Self::Stopping(generation) => Some(generation),
            Self::Down | Self::Waiting | Self::Backoff { .. } | Self::Failed(_) | Self::Exiting => {
                None
            }
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
        self.state = SupervisorState::Waiting;
        Ok(())
    }

    /// Record successful preconditions and allocate the next generation.
    ///
    /// # Errors
    ///
    /// Returns an error unless the lifecycle is waiting, or generation identity is exhausted.
    pub fn preconditions_ready(&mut self) -> Result<Generation, TransitionError> {
        if self.state != SupervisorState::Waiting {
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
        self.state = SupervisorState::Started(generation);
        Ok(())
    }

    /// Record readiness for the current generation.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale generation or invalid current state.
    pub fn child_ready(&mut self, generation: Generation) -> Result<(), TransitionError> {
        if self.state != SupervisorState::Started(generation) {
            return Err(self.invalid("child_ready"));
        }
        self.state = SupervisorState::Ready(generation);
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
                | SupervisorState::Started(current)
                | SupervisorState::Ready(current)
                if current == generation
        ) {
            return Err(self.invalid("begin_stop"));
        }
        self.state = SupervisorState::Stopping(generation);
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
                    | SupervisorState::Started(current)
                    | SupervisorState::Ready(current)
                    if current == generation
            )
        {
            return Err(self.invalid("child_detached"));
        }
        self.state = SupervisorState::Exiting;
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
                | SupervisorState::Started(current)
                | SupervisorState::Ready(current)
                | SupervisorState::Stopping(current)
                if current == generation
        ) {
            return Err(self.invalid("child_reaped"));
        }
        self.state = match decision {
            RestartDecision::Restart {
                base_delay_seconds, ..
            } => SupervisorState::Backoff {
                generation,
                delay_seconds: base_delay_seconds,
            },
            RestartDecision::StayDown => SupervisorState::Down,
            RestartDecision::ExitSupervisor => SupervisorState::Exiting,
            RestartDecision::Fail(reason) => SupervisorState::Failed(reason),
        };
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
            SupervisorState::Exiting
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
            SupervisorState::Waiting | SupervisorState::Backoff { .. }
        ) {
            return Err(self.invalid("cancel_pending"));
        }
        self.state = if matches!(self.desired, DesiredState::Halt | DesiredState::Exit) {
            SupervisorState::Exiting
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
        self.state = SupervisorState::Exiting;
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

    const fn invalid(&self, event: &'static str) -> TransitionError {
        TransitionError::Invalid {
            state: self.state,
            event,
        }
    }
}

/// Policy outcome after a child has been reaped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RestartDecision {
    /// Schedule another generation after applying bounded jitter to the base delay.
    Restart {
        /// Deterministic exponential delay before jitter.
        base_delay_seconds: u64,
        /// Maximum configured jitter percentage.
        jitter_percent: u8,
    },
    /// Keep the supervisor alive without a child.
    StayDown,
    /// Finish the supervisor after cleanup.
    ExitSupervisor,
    /// Enter a configured failure state until reset by an operator.
    Fail(FailureReason),
}

/// Independent retry outcome for a failed pre-start condition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConditionRetry {
    /// Deterministic exponential delay before jitter.
    pub base_delay_seconds: u64,
    /// Maximum configured jitter percentage.
    pub jitter_percent: u8,
}

/// Failure streak for a pre-start condition, separate from service attempts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ConditionTracker {
    failure_streak: u32,
}

impl ConditionTracker {
    /// Record a failed evaluation and select its independent retry delay.
    #[must_use]
    pub fn failed(&mut self, condition: &StartConditionConfig) -> ConditionRetry {
        self.failure_streak = self.failure_streak.saturating_add(1);
        let exponent = self.failure_streak.saturating_sub(1);
        let factor = u64::from(condition.backoff.multiplier).saturating_pow(exponent);
        ConditionRetry {
            base_delay_seconds: condition
                .backoff
                .initial_seconds
                .saturating_mul(factor)
                .min(condition.backoff.max_seconds),
            jitter_percent: condition.backoff.jitter_percent,
        }
    }

    /// Reset condition backoff after one successful evaluation.
    pub const fn passed(&mut self) {
        self.failure_streak = 0;
    }

    /// Number of consecutive condition failures.
    #[must_use]
    pub const fn failure_streak(&self) -> u32 {
        self.failure_streak
    }
}

/// Deterministic history used to enforce restart limits.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RestartTracker {
    first_start_seconds: Option<u64>,
    total_starts: u64,
    short_run_streak: u32,
    recent_starts: VecDeque<u64>,
}

impl RestartTracker {
    /// Total service generations recorded, excluding condition evaluations.
    #[must_use]
    pub const fn total_starts(&self) -> u64 {
        self.total_starts
    }

    /// Record a child start using a monotonic timestamp in seconds.
    pub fn record_start(&mut self, now_seconds: u64) {
        self.first_start_seconds.get_or_insert(now_seconds);
        self.total_starts = self.total_starts.saturating_add(1);
        self.recent_starts.push_back(now_seconds);
    }

    /// Decide what follows a reaped generation.
    ///
    /// `runtime_seconds` is the duration of the generation and `now_seconds` is
    /// monotonic time. Wall-clock changes therefore do not affect supervision.
    #[must_use]
    pub fn decide(
        &mut self,
        result: ChildResult,
        runtime_seconds: u64,
        now_seconds: u64,
        desired: DesiredState,
        restart: &RestartConfig,
    ) -> RestartDecision {
        if matches!(desired, DesiredState::Halt | DesiredState::Exit) {
            return RestartDecision::ExitSupervisor;
        }
        if matches!(desired, DesiredState::Down | DesiredState::Once) {
            return RestartDecision::StayDown;
        }

        let successful = result.is_success(restart);
        let should_restart = match restart.policy {
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => !successful,
            RestartPolicy::Never => false,
        };
        if !should_restart {
            return if restart.exit_when_done {
                RestartDecision::ExitSupervisor
            } else {
                RestartDecision::StayDown
            };
        }

        if let Some(reason) = self.exhausted_limit(now_seconds, restart) {
            return RestartDecision::Fail(reason);
        }

        if runtime_seconds >= restart.backoff.reset_after_seconds {
            self.short_run_streak = 0;
        }
        self.short_run_streak = self.short_run_streak.saturating_add(1);
        RestartDecision::Restart {
            base_delay_seconds: backoff_seconds(self.short_run_streak, restart),
            jitter_percent: restart.backoff.jitter_percent,
        }
    }

    fn exhausted_limit(
        &mut self,
        now_seconds: u64,
        restart: &RestartConfig,
    ) -> Option<FailureReason> {
        if restart
            .limits
            .max_retries
            .is_some_and(|limit| self.total_starts.saturating_sub(1).ge(&u64::from(limit)))
        {
            return Some(FailureReason::RetryLimit);
        }
        if restart.limits.max_elapsed_seconds.is_some_and(|limit| {
            self.first_start_seconds
                .is_some_and(|started| now_seconds.saturating_sub(started) >= limit)
        }) {
            return Some(FailureReason::ElapsedTimeLimit);
        }
        if let Some(burst) = &restart.limits.burst {
            while self
                .recent_starts
                .front()
                .is_some_and(|started| now_seconds.saturating_sub(*started) >= burst.window_seconds)
            {
                self.recent_starts.pop_front();
            }
            if self.recent_starts.len() >= burst.starts as usize {
                return Some(FailureReason::BurstLimit);
            }
        }
        None
    }
}

fn backoff_seconds(streak: u32, restart: &RestartConfig) -> u64 {
    let exponent = streak.saturating_sub(1);
    let factor = u64::from(restart.backoff.multiplier).saturating_pow(exponent);
    restart
        .backoff
        .initial_seconds
        .saturating_mul(factor)
        .min(restart.backoff.max_seconds)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::{
        ChildResult, ConditionTracker, DesiredState, FailureReason, Generation, RestartDecision,
        RestartTracker, StateMachine, SupervisorState,
    };
    use crate::config::{
        ConditionBackoffConfig, RestartBurstLimit, RestartConfig, RestartLimits, RestartPolicy,
        StartConditionConfig,
    };

    #[test]
    fn lifecycle_rejects_stale_generations() -> Result<(), Box<dyn Error>> {
        let mut machine = StateMachine::default();
        machine.begin_start()?;
        let generation = machine.preconditions_ready()?;
        assert_eq!(generation, Generation::FIRST);
        assert!(machine.child_started(Generation(2)).is_err());
        machine.child_started(generation)?;
        machine.child_ready(generation)?;
        machine.set_desired(DesiredState::Down);
        machine.begin_stop(generation)?;
        machine.child_reaped(generation, RestartDecision::StayDown)?;
        assert_eq!(machine.state(), SupervisorState::Down);
        Ok(())
    }

    #[test]
    fn issue_71_on_failure_exits_after_success() {
        let mut tracker = RestartTracker::default();
        tracker.record_start(0);
        let restart = RestartConfig {
            policy: RestartPolicy::OnFailure,
            exit_when_done: true,
            ..RestartConfig::default()
        };
        assert_eq!(
            tracker.decide(ChildResult::Exited(0), 1, 1, DesiredState::Up, &restart),
            RestartDecision::ExitSupervisor
        );
    }

    #[test]
    fn custom_success_code_does_not_restart_on_failure_policy() {
        let mut tracker = RestartTracker::default();
        tracker.record_start(0);
        let mut restart = RestartConfig {
            policy: RestartPolicy::OnFailure,
            ..RestartConfig::default()
        };
        restart.success_exit_codes.insert(2);
        assert_eq!(
            tracker.decide(ChildResult::Exited(2), 1, 1, DesiredState::Up, &restart),
            RestartDecision::StayDown
        );
    }

    #[test]
    fn default_restarts_forever_with_capped_exponential_backoff() {
        let mut tracker = RestartTracker::default();
        let restart = RestartConfig::default();
        let mut delays = Vec::new();
        for now in 0..10 {
            tracker.record_start(now);
            let decision =
                tracker.decide(ChildResult::Signaled(9), 0, now, DesiredState::Up, &restart);
            let RestartDecision::Restart {
                base_delay_seconds, ..
            } = decision
            else {
                return;
            };
            delays.push(base_delay_seconds);
        }
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60, 60]);
    }

    #[test]
    fn max_retries_stops_a_dependency_crash_loop() {
        let mut tracker = RestartTracker::default();
        let restart = RestartConfig {
            limits: RestartLimits {
                max_retries: Some(2),
                ..RestartLimits::default()
            },
            ..RestartConfig::default()
        };

        tracker.record_start(0);
        assert!(matches!(
            tracker.decide(ChildResult::Exited(1), 0, 0, DesiredState::Up, &restart),
            RestartDecision::Restart { .. }
        ));
        tracker.record_start(1);
        assert!(matches!(
            tracker.decide(ChildResult::Exited(1), 0, 1, DesiredState::Up, &restart),
            RestartDecision::Restart { .. }
        ));
        tracker.record_start(2);
        assert_eq!(
            tracker.decide(ChildResult::Exited(1), 0, 2, DesiredState::Up, &restart),
            RestartDecision::Fail(FailureReason::RetryLimit)
        );
    }

    #[test]
    fn elapsed_and_burst_limits_are_distinct() {
        let mut elapsed = RestartTracker::default();
        let elapsed_config = RestartConfig {
            limits: RestartLimits {
                max_elapsed_seconds: Some(10),
                ..RestartLimits::default()
            },
            ..RestartConfig::default()
        };
        elapsed.record_start(2);
        assert_eq!(
            elapsed.decide(
                ChildResult::Exited(1),
                0,
                12,
                DesiredState::Up,
                &elapsed_config,
            ),
            RestartDecision::Fail(FailureReason::ElapsedTimeLimit)
        );

        let mut burst = RestartTracker::default();
        let burst_config = RestartConfig {
            limits: RestartLimits {
                burst: Some(RestartBurstLimit {
                    starts: 3,
                    window_seconds: 10,
                }),
                ..RestartLimits::default()
            },
            ..RestartConfig::default()
        };
        burst.record_start(0);
        burst.record_start(1);
        burst.record_start(2);
        assert_eq!(
            burst.decide(
                ChildResult::Exited(1),
                0,
                2,
                DesiredState::Up,
                &burst_config,
            ),
            RestartDecision::Fail(FailureReason::BurstLimit)
        );
    }

    #[test]
    fn long_healthy_run_resets_backoff() {
        let mut tracker = RestartTracker::default();
        let restart = RestartConfig::default();
        tracker.record_start(0);
        let _ = tracker.decide(ChildResult::Exited(1), 0, 0, DesiredState::Up, &restart);
        tracker.record_start(1);
        let decision = tracker.decide(ChildResult::Exited(1), 60, 61, DesiredState::Up, &restart);
        assert_eq!(
            decision,
            RestartDecision::Restart {
                base_delay_seconds: 1,
                jitter_percent: 20,
            }
        );
    }

    #[test]
    fn once_and_down_never_restart() {
        for desired in [DesiredState::Once, DesiredState::Down] {
            let mut tracker = RestartTracker::default();
            tracker.record_start(0);
            assert_eq!(
                tracker.decide(
                    ChildResult::Exited(1),
                    0,
                    0,
                    desired,
                    &RestartConfig::default(),
                ),
                RestartDecision::StayDown
            );
        }
    }

    #[test]
    fn condition_backoff_never_consumes_service_attempts() {
        let condition = StartConditionConfig {
            command: vec!["/usr/bin/test".to_owned()],
            timeout_seconds: 5,
            backoff: ConditionBackoffConfig {
                initial_seconds: 1,
                max_seconds: 8,
                multiplier: 2,
                jitter_percent: 10,
            },
        };
        let mut conditions = ConditionTracker::default();
        let starts = RestartTracker::default();
        let delays: Vec<u64> = (0..6)
            .map(|_| conditions.failed(&condition).base_delay_seconds)
            .collect();
        assert_eq!(delays, [1, 2, 4, 8, 8, 8]);
        assert_eq!(starts.total_starts(), 0);
        conditions.passed();
        assert_eq!(conditions.failure_streak(), 0);
        assert_eq!(conditions.failed(&condition).base_delay_seconds, 1);
    }

    #[test]
    fn childless_pending_states_cancel_deterministically() -> Result<(), Box<dyn Error>> {
        let mut waiting = StateMachine::default();
        waiting.begin_start()?;
        waiting.set_desired(DesiredState::Halt);
        waiting.cancel_pending()?;
        assert_eq!(waiting.state(), SupervisorState::Exiting);

        let mut backoff = StateMachine::default();
        backoff.begin_start()?;
        let generation = backoff.preconditions_ready()?;
        backoff.child_started(generation)?;
        backoff.child_ready(generation)?;
        backoff.child_reaped(
            generation,
            RestartDecision::Restart {
                base_delay_seconds: 1,
                jitter_percent: 0,
            },
        )?;
        backoff.set_desired(DesiredState::Halt);
        backoff.cancel_pending()?;
        assert_eq!(backoff.state(), SupervisorState::Exiting);

        let mut down = StateMachine::default();
        down.set_desired(DesiredState::Exit);
        down.exit_without_child()?;
        assert_eq!(down.state(), SupervisorState::Exiting);
        Ok(())
    }

    #[test]
    fn live_generation_detaches_only_for_explicit_exit() -> Result<(), Box<dyn Error>> {
        let mut machine = StateMachine::default();
        machine.begin_start()?;
        let generation = machine.preconditions_ready()?;
        machine.child_started(generation)?;
        machine.child_ready(generation)?;
        assert!(machine.child_detached(generation).is_err());
        machine.set_desired(DesiredState::Exit);
        machine.child_detached(generation)?;
        assert_eq!(machine.state(), SupervisorState::Exiting);
        Ok(())
    }
}
