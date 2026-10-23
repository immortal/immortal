//! Descriptor-tracked generation cleanup after the supervisor connection is lost.
//!
//! [`cleanup_after_supervisor_loss`] runs without a supervisor to receive
//! events, so it reaps and discards them. The flow is:
//!
//! 1. Kill every live generation, through its group while its armed guard or
//!    member leader pins it, retire the group, and wait, up to
//!    [`SHUTDOWN_KILL_WAIT`], until no workload process is owned. A descriptor
//!    group whose launcher was reaped is left for its stop command.
//! 2. If exactly one descriptor-tracked generation remains and a
//!    [`BrokerLifetimePlan`] was configured, run its stop command. A hook which
//!    overruns its stop timeout is killed the same way and its group retired.
//! 3. Wait for the tracked generation's lifetime descriptor to close. That
//!    completes the generation, which retires its group without a signal: a
//!    member which closed the descriptor may still be exiting. A hook overrun
//!    whose own group never drains returns before this step, so the service
//!    group is not retired then and its armed guard still contains it.
//! 4. On every path, wait up to [`SHUTDOWN_KILL_WAIT`] until every retired
//!    group has drained.
//!
//! Draining is deliberately last: a killed group held by a process outside the
//! broker's reach must not stop the configured stop command from ending a
//! tracked service, so earlier stages wait only for owned workloads. Every wait
//! drains through [`collect_child_events`] and wakes through
//! [`wait_for_change`], so members reparented to the broker are reaped and a
//! group drained by another parent is still noticed. The broker treats any
//! survivor, timeout, or an ambiguous multi-generation state as a hard failure
//! rather than guessing at owner intent. Each stage starts its deadline before
//! any signal sweep; helper exchanges and foreground-reap kills share that
//! deadline with settling. An expired budget skips delivery, never adds another
//! per-helper wait, and still retires killed groups.

use std::io;

use tokio::signal::unix::Signal as ChildSignal;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout_at};

use crate::supervisor::Generation;

use super::error::ProcessBrokerError;
use super::reap::{
    GroupPin, collect_child_events, has_workload_processes, kill_every_group, kill_live_group,
};
use super::runtime::record_lifetime_observation;
use super::settle::{POLL_START, wait_for_change};
use super::shutdown::{SHUTDOWN_GRACE, SHUTDOWN_KILL_WAIT};
use super::spawn_process;
use super::state::{BrokerOwnedProcess, BrokerRuntimeState, LifetimeObservation, LifetimeState};
use super::types::BrokerLifetimePlan;

/// A descriptor-tracked generation paired with the plan for cleaning it up
/// after the supervisor connection is lost.
struct SupervisorLossCleanup {
    generation: Generation,
    plan: BrokerLifetimePlan,
}

/// Fail closed after the supervisor connection is gone.
///
/// A lost supervisor cannot make restart decisions or reap children, so the
/// broker stops every generation it owns, disarms its guards, consumes the
/// remaining descriptor-lifetime observations so no tracked descriptor
/// outlives the process, and finally waits for every group it killed or
/// released to drain. This is a deliberate kill path: leaving a supervised
/// workload running without a supervisor would leave an unmanaged process on
/// the host.
///
/// # Errors
///
/// Returns an error when a stop, reap, drain, or cleanup step leaves ownership
/// unresolved; a stop failure takes precedence over a drain timeout. The caller
/// exits the broker either way, so the error is diagnostic rather than
/// recoverable.
pub(super) async fn cleanup_after_supervisor_loss(
    child_signal: &mut ChildSignal,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    state: &mut BrokerRuntimeState,
) -> Result<(), ProcessBrokerError> {
    let stopped = stop_after_supervisor_loss(child_signal, lifetime_events, state).await;
    // Whatever stopping concluded, give every killed or released group a
    // bounded chance to drain so none of its members reparents after exit.
    let deadline = Instant::now() + SHUTDOWN_KILL_WAIT;
    let drained = reap_without_events(deadline, child_signal, state, groups_drained).await;
    state.signal_deadline = None;
    stopped?;
    if drained? {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "killed process groups survived supervisor-loss cleanup",
        )
        .into())
    }
}

/// Kill live groups, then run the descriptor stop hook and await closure.
async fn stop_after_supervisor_loss(
    child_signal: &mut ChildSignal,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    state: &mut BrokerRuntimeState,
) -> Result<(), ProcessBrokerError> {
    let deadline = Instant::now() + SHUTDOWN_KILL_WAIT;
    state.signal_deadline = Some(deadline.into_std());
    kill_every_group(
        &state.generations,
        &mut state.retiring,
        state.signal_deadline,
    );
    if !reap_without_events(deadline, child_signal, state, workloads_reaped).await? {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "broker children survived supervisor-loss cleanup",
        )
        .into());
    }

    let mut tracked = state.generations.iter().filter_map(|(generation, entry)| {
        (entry.lifetime != LifetimeState::Foreground).then_some(*generation)
    });
    let generation = tracked.next();
    if tracked.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker owns more than one descriptor-tracked generation",
        )
        .into());
    }
    let complete = match (generation, state.lifetime_cleanup.take()) {
        (Some(generation), Some(plan)) => {
            run_supervisor_loss_hook(
                SupervisorLossCleanup { generation, plan },
                child_signal,
                lifetime_events,
                state,
            )
            .await?
        }
        (None, _) => true,
        (Some(_), None) => false,
    };
    if complete {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "descriptor cleanup did not establish lifetime closure",
        )
        .into())
    }
}

/// Run the stop hook, then await the tracked generation's lifetime closure.
///
/// Returns whether the hook finished on its own and the lifetime closed.
/// Closure completes the generation, and completion retires its group, even
/// when the hook overran, so the final drain awaits any member still exiting.
///
/// # Errors
///
/// Returns a reap or lifetime-stream failure.
async fn run_supervisor_loss_hook(
    cleanup: SupervisorLossCleanup,
    child_signal: &mut ChildSignal,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    state: &mut BrokerRuntimeState,
) -> Result<bool, ProcessBrokerError> {
    let SupervisorLossCleanup { generation, plan } = cleanup;
    let BrokerLifetimePlan {
        stop,
        stop_timeout,
        lifetime_timeout,
    } = plan;
    let hook = match spawn_process(stop, SHUTDOWN_GRACE) {
        Ok(hook) => {
            state
                .processes
                .insert(hook.process(), BrokerOwnedProcess::Generation(generation));
            Some(hook)
        }
        Err(error) => {
            if let Some(process) = error.cleanup_pending() {
                state
                    .processes
                    .insert(process, BrokerOwnedProcess::Generation(generation));
            }
            None
        }
    };
    let deadline = Instant::now() + stop_timeout;
    let hook_finished =
        reap_without_events(deadline, child_signal, state, workloads_reaped).await?;
    if let Some(hook) = hook
        && !hook_finished
    {
        // The overrunning hook still pins its group unless it was reaped while
        // another owned process kept the wait open; then the group is awaited.
        // No generation can register during supervisor-loss cleanup, so no
        // other generation can have reused the hook's group ID. The hook has
        // no guard, so membership plus its unreaped PID == PGID is required.
        let deadline = Instant::now() + SHUTDOWN_KILL_WAIT;
        state.signal_deadline = Some(deadline.into_std());
        if state.processes.contains_key(&hook.process()) {
            let pin = GroupPin {
                armed: false,
                reused: false,
            };
            let _ = kill_live_group(
                hook.process(),
                hook.group(),
                pin,
                None,
                &mut state.retiring,
                state.signal_deadline,
            );
        } else {
            state.retiring.retire(hook.group());
        }
        if !reap_without_events(deadline, child_signal, state, workloads_reaped).await? {
            return Ok(false);
        }
    }
    // Closure completes the generation and retires its group, so the final
    // drain waits for a member which closed the descriptor but is still exiting.
    let lifetime_closed = wait_for_cleanup_lifetime(
        generation,
        Instant::now() + lifetime_timeout,
        lifetime_events,
        state,
    )
    .await?;
    Ok(hook_finished && lifetime_closed)
}

/// Whether every owned workload process, including a stop hook, was reaped.
fn workloads_reaped(state: &BrokerRuntimeState) -> bool {
    !has_workload_processes(&state.processes)
}

/// Whether, in addition, every killed or released group has drained.
fn groups_drained(state: &BrokerRuntimeState) -> bool {
    workloads_reaped(state) && state.retiring.is_empty()
}

/// Reap and discard events until `settled` holds or `deadline` passes.
async fn reap_without_events(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    state: &mut BrokerRuntimeState,
    settled: fn(&BrokerRuntimeState) -> bool,
) -> Result<bool, ProcessBrokerError> {
    state.signal_deadline = Some(deadline.into_std());
    let mut poll = POLL_START;
    loop {
        let _ = collect_child_events(state)?;
        if settled(state) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        wait_for_change(deadline, child_signal, &mut poll).await?;
    }
}

async fn wait_for_cleanup_lifetime(
    generation: Generation,
    deadline: Instant,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    state: &mut BrokerRuntimeState,
) -> Result<bool, ProcessBrokerError> {
    state.signal_deadline = Some(deadline.into_std());
    if !state.generations.contains_key(&generation) {
        return Ok(true);
    }
    loop {
        let observation = match timeout_at(deadline, lifetime_events.recv()).await {
            Ok(Some(observation)) => observation,
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "broker lifetime event stream closed",
                )
                .into());
            }
            Err(_) => return Ok(false),
        };
        let observed_generation = observation.generation;
        let closed = observation.result.is_ok();
        let _ = record_lifetime_observation(
            &observation,
            &mut state.generations,
            &mut state.processes,
            &mut state.retiring,
        )?;
        if observed_generation == generation {
            return Ok(closed && !state.generations.contains_key(&generation));
        }
    }
}
