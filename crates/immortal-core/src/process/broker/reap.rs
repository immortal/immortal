//! Child reaping, containment-guard disarming, and group-wide signaling.
//!
//! [`collect_child_events`]/[`forward_child_events`] drain every pending
//! `SIGCHLD`-driven wait, correlating each reaped process against its owning
//! generation or containment guard: a guard reaped while its generation is
//! still owned is a containment failure and kills the group, while an
//! ordinary workload exit only removes bookkeeping once its guard is
//! disarmed. [`disarm_generation_guard`] is the single place that closes the
//! guard's disarm endpoint and reconciles its process-ownership entry.

use std::collections::BTreeMap;
use std::io;

use tokio::io::AsyncWrite;

use crate::supervisor::Generation;

use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::spawn::GROUP_GUARD_CLEANUP_TIMEOUT;
use super::state::{BrokerGeneration, BrokerOwnedProcess, LifetimeState};
use super::wire::write_event;
use super::{BrokerEvent, ProcessId, ProcessSignal, SignalTarget, reap_any_event, signal};

pub(super) async fn forward_child_events<W>(
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    for event in collect_child_events(generations, processes, logging)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

pub(super) fn collect_child_events(
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
) -> Result<Vec<BrokerEvent>, ProcessBrokerError> {
    let mut events = Vec::new();
    if processes.is_empty() {
        return Ok(events);
    }
    while !processes.is_empty() {
        let Some(event) = reap_any_event()? else {
            break;
        };
        let process = event.pid();
        let owned = processes.get(&process).copied().ok_or(ProcessBrokerError(
            ProcessBrokerErrorKind::UnownedChild(process),
        ))?;
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
