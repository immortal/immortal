//! Bounded terminate-then-kill shutdown of every broker-owned generation.
//!
//! The broker signals a process group only while something pins its numeric ID
//! to the group the broker created: the generation's armed guard, whose
//! unreaped anchor is a member until the guard is disarmed, or an unreaped
//! direct child which is still a member and whose PID equals the PGID. Every
//! other group it owns is awaited without a signal. The flow is:
//!
//! 1. `SIGTERM` every live generation, through its group while it is pinned
//!    and otherwise through the child alone, and every descriptor group whose
//!    guard is still armed.
//! 2. Drain and re-check until [`SHUTDOWN_GRACE`] expires: forward reaps,
//!    apply pending lifetime observations, and finish once no workload, no
//!    tracked generation with an open lifetime descriptor, and no retiring
//!    group remains.
//! 3. Otherwise `SIGKILL` the same targets and retire each group.
//! 4. Drain and re-check the same condition until [`SHUTDOWN_KILL_WAIT`]
//!    expires.
//!
//! A tracked generation ends only when its lifetime descriptor closes, never
//! because its group looks empty: a daemon which calls `setsid` leaves its
//! reserved group while it still holds the descriptor, and that escaped daemon
//! is beyond any group signal. Its completion retires the group as usual. A generation whose descriptor stays open keeps
//! shutdown from completing and stays owned, with its guard armed, so
//! supervisor-loss cleanup can still run its stop command. An incomplete
//! shutdown keeps every remaining generation, guard, and retiring group.
//!
//! Each phase's deadline starts before its signal sweep. Helper round trips,
//! including foreground-reap kills during draining, share its remaining budget;
//! expired calls are skipped rather than starting a fresh per-helper wait.
//! Delivery plus settling therefore retains the two-plus-three-second budget,
//! independent of helper count. An unacknowledged foreground kill must instead
//! release its guard fail-closed, adding that helper's three-second cleanup
//! deadline rather than disarming live members. Final kernel reaping and event
//! writes also depend on kernel/scheduler progress, not a hard real-time guarantee.
//! Supervisor-loss stages use the same delivery-and-settle deadline discipline.

use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::signal::unix::Signal as ChildSignal;
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::ProcessSignal;
use super::error::ProcessBrokerError;
use super::reap::{
    forward_child_events, has_workload_processes, kill_every_group, signal_descriptor_groups,
    terminate_every_group,
};
use super::runtime::record_lifetime_observation;
use super::settle::{POLL_START, wait_for_change};
use super::state::{BrokerGeneration, BrokerRuntimeState, LifetimeObservation, LifetimeState};
use super::wire::write_event;

pub(super) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
pub(super) const SHUTDOWN_KILL_WAIT: Duration = Duration::from_secs(3);
/// Total signal/settle budget before an ordered shutdown answers its supervisor.
/// Kernel-dependent final reaping and event writes are not hard real-time bounded.
pub(crate) const BROKER_SHUTDOWN_BOUND: Duration =
    SHUTDOWN_GRACE.saturating_add(SHUTDOWN_KILL_WAIT);

/// How an ordered shutdown ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ShutdownOutcome {
    /// No workload, owned group, or retiring group remains.
    Complete,
    /// The kill wait expired first; `os_error` is the last group-probe failure.
    Failed { os_error: Option<i32> },
}

/// Stop every owned generation in order, escalating only if it is required.
///
/// Signals each live generation and armed descriptor group with `SIGTERM`,
/// keeps forwarding child events to the supervisor so the reaps are not lost,
/// and waits up to [`SHUTDOWN_GRACE`] for every workload to exit, every tracked
/// lifetime to close, and every retiring group to drain. The same targets are
/// then killed, and the same condition must hold within [`SHUTDOWN_KILL_WAIT`].
/// Lifetime observations are applied and forwarded like reaps, so the
/// supervisor never loses one, even when shutdown fails. Guards are disarmed
/// rather than killed, so a workload which exits normally during the grace
/// window is never escalated. A failed shutdown keeps all remaining ownership
/// for the caller.
///
/// # Errors
///
/// Returns an error when waiting, forwarding, or disarming fails in a way
/// which leaves ownership unresolved, which the caller treats as a broker
/// fault rather than a clean shutdown.
pub(super) async fn shutdown_owned<W>(
    child_signal: &mut ChildSignal,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    writer: &mut W,
    state: &mut BrokerRuntimeState,
) -> Result<ShutdownOutcome, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let result = shutdown_phases(child_signal, lifetime_events, writer, state).await;
    state.signal_deadline = None;
    result
}

/// Spend each phase's deadline on both its signal sweep and its settle loop.
async fn shutdown_phases<W>(
    child_signal: &mut ChildSignal,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    writer: &mut W,
    state: &mut BrokerRuntimeState,
) -> Result<ShutdownOutcome, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let grace = Instant::now() + SHUTDOWN_GRACE;
    state.signal_deadline = Some(grace.into_std());
    terminate_every_group(
        &state.generations,
        &mut state.retiring,
        state.signal_deadline,
    );
    signal_descriptor_groups(
        &state.generations,
        ProcessSignal::Terminate,
        &mut state.retiring,
        state.signal_deadline,
    );
    let mut events = ShutdownEvents {
        child_signal,
        lifetime_events,
        writer,
    };
    if settle_until(grace, &mut events, state).await? {
        return Ok(ShutdownOutcome::Complete);
    }
    let kill = Instant::now() + SHUTDOWN_KILL_WAIT;
    state.signal_deadline = Some(kill.into_std());
    kill_every_group(
        &state.generations,
        &mut state.retiring,
        state.signal_deadline,
    );
    signal_descriptor_groups(
        &state.generations,
        ProcessSignal::Kill,
        &mut state.retiring,
        state.signal_deadline,
    );
    if settle_until(kill, &mut events, state).await? {
        return Ok(ShutdownOutcome::Complete);
    }
    Ok(ShutdownOutcome::Failed {
        os_error: state.retiring.last_probe_error(),
    })
}

/// The event sources and write endpoint a shutdown settle loop drives.
struct ShutdownEvents<'a, W> {
    child_signal: &'a mut ChildSignal,
    lifetime_events: &'a mut mpsc::Receiver<LifetimeObservation>,
    writer: &'a mut W,
}

/// Drain and re-check until shutdown has settled or `deadline` passes.
async fn settle_until<W>(
    deadline: Instant,
    events: &mut ShutdownEvents<'_, W>,
    state: &mut BrokerRuntimeState,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let mut poll = POLL_START;
    loop {
        forward_child_events(events.writer, state).await?;
        while let Ok(observation) = events.lifetime_events.try_recv() {
            if let Some(event) = record_lifetime_observation(
                &observation,
                &mut state.generations,
                &mut state.processes,
                &mut state.retiring,
            )? {
                write_event(events.writer, &event).await?;
            }
        }
        if is_settled(state) {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        wait_for_change(deadline, events.child_signal, &mut poll).await?;
    }
}

/// Whether no workload, open tracked lifetime, or retiring group remains.
fn is_settled(state: &BrokerRuntimeState) -> bool {
    !has_workload_processes(&state.processes)
        && !state.generations.values().any(awaits_lifetime_closure)
        && state.retiring.is_empty()
}

/// Whether `entry` is a tracked generation whose launcher was reaped but whose
/// lifetime descriptor is still open.
///
/// The service is still running by its own contract, whatever its group's
/// membership: only the descriptor's closure completes it.
fn awaits_lifetime_closure(entry: &BrokerGeneration) -> bool {
    entry.child.is_none() && entry.lifetime == LifetimeState::Tracking
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::super::ProcessGroupId;
    use super::super::state::{BrokerGeneration, LifetimeState};
    use super::awaits_lifetime_closure;

    fn generation(lifetime: LifetimeState) -> Result<BrokerGeneration, Box<dyn Error>> {
        Ok(BrokerGeneration {
            child: None,
            guard: None,
            group: ProcessGroupId::try_from(4_242)?,
            lifetime,
        })
    }

    /// Only the lifetime descriptor's closure completes a tracked generation,
    /// so an open one keeps shutdown waiting whatever its guard or group; a
    /// closed, failed, or foreground generation does not.
    #[test]
    fn only_an_open_tracked_lifetime_keeps_shutdown_waiting() -> Result<(), Box<dyn Error>> {
        assert!(awaits_lifetime_closure(&generation(
            LifetimeState::Tracking
        )?));
        for lifetime in [
            LifetimeState::Closed,
            LifetimeState::Failed,
            LifetimeState::Foreground,
        ] {
            assert!(!awaits_lifetime_closure(&generation(lifetime)?));
        }
        Ok(())
    }
}
