//! Child reaping, containment-guard disarming, and group-wide signaling.
//!
//! [`collect_child_events`]/[`forward_child_events`] drain every pending
//! `SIGCHLD`-driven wait, correlating each reaped process against its owning
//! generation or containment guard: a guard reaped while its generation is
//! still owned is a containment failure and kills the group, while an
//! ordinary workload exit only removes bookkeeping once its guard is
//! disarmed. [`disarm_generation_guard`] is the single place that closes the
//! guard's disarm endpoint and reconciles its process-ownership entry.
//!
//! When the broker holds the child-subreaper role, orphaned descendants of the
//! supervised subtree also reparent to it; [`collect_child_events`] reaps those
//! adopted orphans for hygiene without ever forwarding a workload event. An
//! unowned reap while the role is absent remains a fatal ownership violation.

use std::collections::BTreeMap;
use std::io;

use tokio::io::AsyncWrite;

use crate::supervisor::Generation;

use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::spawn::GROUP_GUARD_CLEANUP_TIMEOUT;
use super::state::{BrokerGeneration, BrokerOwnedProcess, LifetimeState};
use super::wire::write_event;
use super::{
    BrokerEvent, ChildEvent, ProcessId, ProcessSignal, SignalTarget, reap_any_event, signal,
};

pub(super) async fn forward_child_events<W>(
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    subreaper_active: bool,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    for event in collect_child_events(generations, processes, logging, subreaper_active)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

pub(super) fn collect_child_events(
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    subreaper_active: bool,
) -> Result<Vec<BrokerEvent>, ProcessBrokerError> {
    let mut events = Vec::new();
    while let Some(event) = next_reaped_event()? {
        let process = event.pid();
        let Some(owned) = processes.get(&process).copied() else {
            reap_adopted_orphan(process, event, subreaper_active)?;
            continue;
        };
        if event.is_terminal() {
            processes.remove(&process);
        }
        match owned {
            BrokerOwnedProcess::Generation(generation) => {
                if event.is_terminal() {
                    let remove_generation = if let Some(entry) = generations.get_mut(&generation) {
                        if entry.lifetime == LifetimeState::Foreground {
                            let _ = signal(SignalTarget::Group(entry.group), ProcessSignal::Kill);
                            true
                        } else {
                            entry.child = None;
                            matches!(
                                entry.lifetime,
                                LifetimeState::Closed | LifetimeState::Failed
                            )
                        }
                    } else {
                        false
                    };
                    if remove_generation {
                        disarm_generation_guard(generation, generations, processes)?;
                        generations.remove(&generation);
                        logging.child_reaped(generation);
                    }
                }
                events.push(BrokerEvent::Child { generation, event });
            }
            BrokerOwnedProcess::Guard(generation) => {
                processes.remove(&process);
                let group = generations.get_mut(&generation).map(|entry| {
                    drop(entry.guard.take());
                    entry.group
                });
                if let Some(group) = group {
                    let _ = signal(SignalTarget::Group(group), ProcessSignal::Kill);
                }
                events.push(BrokerEvent::ContainmentFailed { generation });
            }
        }
    }
    Ok(events)
}

/// Drain the next reaped event, treating an empty subtree as sweep completion.
///
/// [`reap_any_event`] surfaces the operating system's `ECHILD` as an error so
/// probes can prove that no child was left behind. Inside the broker's own
/// drain that same signal only means every waitable child has been reaped, so
/// it folds to `Ok(None)`. This lets the loop keep draining until the subtree
/// is empty — routine once the broker holds the subreaper role and sweeps with
/// no owned processes — without collapsing the distinction the primitive keeps
/// for its callers.
fn next_reaped_event() -> Result<Option<ChildEvent>, ProcessBrokerError> {
    match reap_any_event() {
        Ok(event) => Ok(event),
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// Account for a reaped process the broker never spawned.
///
/// Once the broker holds the child-subreaper role, orphaned grandchildren of
/// supervised services reparent to it and surface here as unowned reaps. The
/// wait already reaped the zombie, so containment only records the terminal
/// reap as a diagnostic; an adopted orphan is never a workload event and never
/// reaches restart policy. Without the subreaper role an unowned reap still
/// violates the broker's ownership invariant and stays a hard error.
///
/// # Errors
///
/// Returns an unowned-child error when the broker is not a subreaper, which
/// preserves the ownership check on platforms without the role.
fn reap_adopted_orphan(
    process: ProcessId,
    event: ChildEvent,
    subreaper_active: bool,
) -> Result<(), ProcessBrokerError> {
    if !subreaper_active {
        return Err(ProcessBrokerError(ProcessBrokerErrorKind::UnownedChild(
            process,
        )));
    }
    if event.is_terminal() {
        eprintln!("immortal process broker: reaped adopted orphan {process}");
    }
    Ok(())
}

pub(super) fn disarm_generation_guard(
    generation: Generation,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<(), ProcessBrokerError> {
    let Some(guard) = generations
        .get_mut(&generation)
        .and_then(|entry| entry.guard.take())
    else {
        return Ok(());
    };
    let process = guard.process()?;
    if processes.remove(&process) != Some(BrokerOwnedProcess::Guard(generation)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process-group guard ownership is inconsistent",
        )
        .into());
    }
    guard.disarm(GROUP_GUARD_CLEANUP_TIMEOUT)?;
    Ok(())
}

pub(super) fn signal_every_group(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    requested: ProcessSignal,
) {
    for child in generations.values().filter_map(|entry| entry.child) {
        let _ = signal(SignalTarget::Group(child.group()), requested);
    }
}

pub(super) fn has_workload_processes(processes: &BTreeMap<ProcessId, BrokerOwnedProcess>) -> bool {
    processes
        .values()
        .any(|owned| matches!(owned, BrokerOwnedProcess::Generation(_)))
}
