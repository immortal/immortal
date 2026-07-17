//! State-machine and restart-policy behavior tests.

use std::error::Error;

use super::{
    ChildResult, ConditionTracker, DesiredState, FailureReason, Generation, RestartDecision,
    RestartTracker, StateMachine, SupervisorState, TransitionError,
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
fn rejected_events_preserve_entire_lifecycle() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    assert_rejected_without_mutation(&mut machine, StateMachine::preconditions_ready);
    assert_rejected_without_mutation(&mut machine, |current| {
        current.child_started(Generation::FIRST)
    });
    assert_rejected_without_mutation(&mut machine, |current| {
        current.child_reaped(Generation::FIRST, RestartDecision::StayDown)
    });

    machine.begin_start()?;
    let generation = machine.preconditions_ready()?;
    machine.child_started(generation)?;
    machine.child_ready(generation)?;
    let stale = Generation(generation.get().saturating_add(1));

    assert_rejected_without_mutation(&mut machine, StateMachine::begin_start);
    assert_rejected_without_mutation(&mut machine, StateMachine::preconditions_ready);
    assert_rejected_without_mutation(&mut machine, |current| current.child_started(stale));
    assert_rejected_without_mutation(&mut machine, |current| current.child_ready(stale));
    assert_rejected_without_mutation(&mut machine, |current| current.child_paused(stale));
    assert_rejected_without_mutation(&mut machine, |current| current.child_continued(generation));
    assert_rejected_without_mutation(&mut machine, |current| current.begin_stop(stale));
    assert_rejected_without_mutation(&mut machine, |current| {
        current.child_reaped(stale, RestartDecision::StayDown)
    });
    assert_rejected_without_mutation(&mut machine, |current| current.child_completed(stale));
    assert_rejected_without_mutation(&mut machine, |current| current.backoff_elapsed(stale));
    Ok(())
}

fn assert_rejected_without_mutation<T>(
    machine: &mut StateMachine,
    operation: impl FnOnce(&mut StateMachine) -> Result<T, TransitionError>,
) {
    let before = machine.clone();
    assert!(operation(machine).is_err());
    assert_eq!(*machine, before);
}

#[test]
fn initialization_completes_exactly_once() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::initializing(DesiredState::Down);
    assert_eq!(machine.state(), SupervisorState::Initializing);
    assert_eq!(machine.desired(), DesiredState::Down);

    machine.initialized()?;
    assert_eq!(machine.state(), SupervisorState::Down);
    assert!(machine.initialized().is_err());
    Ok(())
}

#[test]
fn childless_failure_requires_down_and_can_be_reset() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.fail_without_child(FailureReason::LoggerRetryLimit)?;
    assert_eq!(
        machine.state(),
        SupervisorState::Failed(FailureReason::LoggerRetryLimit)
    );
    assert!(
        machine
            .fail_without_child(FailureReason::LoggerRetryLimit)
            .is_err()
    );

    machine.reset_failure()?;
    assert_eq!(machine.state(), SupervisorState::Down);
    assert_eq!(machine.desired(), DesiredState::Up);
    Ok(())
}

#[test]
fn pause_and_continue_preserve_readiness() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.begin_start()?;
    let generation = machine.preconditions_ready()?;
    machine.child_started(generation)?;

    machine.child_paused(generation)?;
    assert_eq!(
        machine.state(),
        SupervisorState::Paused {
            generation,
            ready: false,
        }
    );
    machine.child_continued(generation)?;
    assert_eq!(machine.state(), SupervisorState::Running(generation));

    machine.child_ready(generation)?;
    machine.child_paused(generation)?;
    assert_eq!(
        machine.state(),
        SupervisorState::Paused {
            generation,
            ready: true,
        }
    );
    assert!(machine.child_continued(Generation(2)).is_err());
    machine.child_continued(generation)?;
    assert_eq!(machine.state(), SupervisorState::Ready(generation));
    Ok(())
}

#[test]
fn aborted_descriptor_stop_restores_live_state_and_intent() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.begin_start()?;
    let generation = machine.preconditions_ready()?;
    machine.child_started(generation)?;
    machine.child_ready(generation)?;
    machine.set_desired(DesiredState::Halt);
    machine.begin_stop(generation)?;

    machine.abort_stop(generation, true, DesiredState::Up)?;
    assert_eq!(machine.state(), SupervisorState::Ready(generation));
    assert_eq!(machine.desired(), DesiredState::Up);
    assert!(
        machine
            .abort_stop(generation, true, DesiredState::Up)
            .is_err()
    );
    Ok(())
}

#[test]
fn completed_generation_retains_identity_until_hook_finishes() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.begin_start()?;
    let generation = machine.preconditions_ready()?;
    machine.child_started(generation)?;

    machine.child_completed(generation)?;
    assert_eq!(machine.state(), SupervisorState::Completed(generation));
    assert_eq!(machine.state().generation(), Some(generation));
    assert_eq!(machine.state().live_generation(), None);
    assert!(machine.child_completed(Generation(2)).is_err());

    machine.completion_finished(
        generation,
        RestartDecision::Restart {
            base_delay_seconds: 3,
            jitter_percent: 0,
        },
    )?;
    assert_eq!(
        machine.state(),
        SupervisorState::Backoff {
            generation,
            delay_seconds: 3,
        }
    );
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
        let decision = tracker.decide(ChildResult::Signaled(9), 0, now, DesiredState::Up, &restart);
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
fn exit_when_done_preserves_exhausted_retry_failure() {
    let mut tracker = RestartTracker::default();
    let restart = RestartConfig {
        exit_when_done: true,
        limits: RestartLimits {
            max_retries: Some(0),
            ..RestartLimits::default()
        },
        ..RestartConfig::default()
    };

    tracker.record_start(0);
    assert_eq!(
        tracker.decide(ChildResult::Exited(1), 0, 0, DesiredState::Up, &restart),
        RestartDecision::ExitFailure(FailureReason::RetryLimit)
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
    assert_eq!(waiting.state(), SupervisorState::Exited);

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
    assert_eq!(backoff.state(), SupervisorState::Exited);

    let mut down = StateMachine::default();
    down.set_desired(DesiredState::Exit);
    down.exit_without_child()?;
    assert_eq!(down.state(), SupervisorState::Exited);
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
    assert_eq!(machine.state(), SupervisorState::Exited);
    Ok(())
}
