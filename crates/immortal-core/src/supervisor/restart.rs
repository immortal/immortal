//! Restart decision and rate-limit policy.
//!
//! These types decide whether a reaped child should restart, stay down, or
//! fail permanently, and enforce the configured retry, elapsed-time, and burst
//! limits together with start-condition backoff. The policy is pure: it maps
//! child results and clock readings onto a [`RestartDecision`] without I/O.

use std::collections::VecDeque;

use crate::config::{RestartConfig, RestartPolicy, StartConditionConfig};

use super::{ChildResult, DesiredState, FailureReason};

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
    /// Finish after cleanup while preserving the exhausted restart limit as failure.
    ExitFailure(FailureReason),
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
            return if restart.exit_when_done {
                RestartDecision::ExitFailure(reason)
            } else {
                RestartDecision::Fail(reason)
            };
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
