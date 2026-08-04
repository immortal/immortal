//! Child reaping, containment-guard disarming, and group-wide signaling.
//!
//! [`collect_child_events`]/[`forward_child_events`] drain every pending
//! `SIGCHLD`-driven wait, correlating each reaped process against its owning
//! generation or containment guard: a guard that *exits* while its generation
//! is still owned is a containment failure and kills the group, while an
//! ordinary workload exit only removes bookkeeping once its guard is
//! disarmed. Waits also report job-control stops and resumes, which change no
//! ownership and are therefore ignored on both paths. [`disarm_generation_guard`]
//! is the single place that closes the
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
        if let Some(event) = correlate_owned_event(event, owned, generations, processes, logging)? {
            events.push(event);
        }
    }
    Ok(events)
}

/// Translate one reaped event for a process this broker owns.
///
/// Returns the event to forward, or `None` when the state change carries no
/// ownership consequence. Reconciles generation bookkeeping and containment as
/// a side effect: a workload exit may retire its generation and disarm its
/// guard, and a guard exit kills the group it was containing.
fn correlate_owned_event(
    event: ChildEvent,
    owned: BrokerOwnedProcess,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
) -> Result<Option<BrokerEvent>, ProcessBrokerError> {
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
            Ok(Some(BrokerEvent::Child { generation, event }))
        }
        // Only a guard that actually exited has stopped containing the group.
        // Waits also report job-control stops and resumes, and treating those
        // as containment failure would kill a healthy workload the moment its
        // guard was suspended.
        BrokerOwnedProcess::Guard(generation) if event.is_terminal() => {
            let group = generations.get_mut(&generation).map(|entry| {
                drop(entry.guard.take());
                entry.group
            });
            if let Some(group) = group {
                let _ = signal(SignalTarget::Group(group), ProcessSignal::Kill);
            }
            Ok(Some(BrokerEvent::ContainmentFailed { generation }))
        }
        BrokerOwnedProcess::Guard(_) => Ok(None),
    }
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

/// Release a generation's process-group guard and its ownership record.
///
/// Takes the guard, drops the broker's ownership of the guard process, and
/// disarms it under a bounded timeout so the guard exits without killing a
/// workload which finished normally. Absence is not an error: a generation
/// which never armed a guard, or whose guard was already disarmed, is a
/// no-op so shutdown and reap paths can both call this.
///
/// # Errors
///
/// Returns an error when the guard's process identity is unavailable, when
/// the ownership table disagrees about who owns the guard, or when disarming
/// fails. An ownership disagreement means the broker's containment invariant
/// is already broken, so it is reported rather than ignored.
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

/// Send one signal to every live generation's whole process group.
///
/// Delivery failures are ignored on purpose: a group which already exited is
/// the outcome the caller wanted, and shutdown must continue signalling the
/// remaining groups regardless.
pub(super) fn signal_every_group(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    requested: ProcessSignal,
) {
    for child in generations.values().filter_map(|entry| entry.child) {
        let _ = signal(SignalTarget::Group(child.group()), requested);
    }
}

/// Report whether any supervised workload process is still owned.
///
/// Guard processes are deliberately excluded, so shutdown waits for real
/// workloads rather than for the containment guards which outlive them.
pub(super) fn has_workload_processes(processes: &BTreeMap<ProcessId, BrokerOwnedProcess>) -> bool {
    processes
        .values()
        .any(|owned| matches!(owned, BrokerOwnedProcess::Generation(_)))
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use super::super::logging::BrokerLoggingPlan;
    use super::super::state::BrokerOwnedProcess;
    use super::{
        BTreeMap, BrokerEvent, BrokerLogging, ChildEvent, ProcessId, correlate_owned_event,
    };
    use crate::supervisor::Generation;

    const GUARD_PID: i32 = 4_242;

    fn guard_process() -> Result<ProcessId, Box<dyn Error>> {
        ProcessId::new(GUARD_PID).ok_or_else(|| "invalid test process id".into())
    }

    fn test_generation() -> Result<Generation, Box<dyn Error>> {
        Generation::new(1).ok_or_else(|| "invalid test generation".into())
    }

    fn empty_logging() -> Result<BrokerLogging, Box<dyn Error>> {
        Ok(BrokerLogging::prepare(BrokerLoggingPlan {
            local_files: Vec::new(),
            logger: None,
        })?)
    }

    /// Regression: a suspended guard is not a containment failure.
    ///
    /// Waits report job-control stops and resumes alongside exits. The guard
    /// arm acted on every event, so delivering `SIGSTOP` to the guard killed
    /// the workload group and reported containment failure, which the executor
    /// escalates into a supervisor abort.
    #[test]
    fn a_stopped_guard_is_not_a_containment_failure() -> Result<(), Box<dyn Error>> {
        let generation = test_generation()?;
        let process = guard_process()?;
        let mut generations = BTreeMap::new();
        let mut processes = BTreeMap::from([(process, BrokerOwnedProcess::Guard(generation))]);
        let mut logging = empty_logging()?;

        for event in [
            ChildEvent::Stopped {
                pid: process,
                signal: 19,
            },
            ChildEvent::Continued { pid: process },
        ] {
            let forwarded = correlate_owned_event(
                event,
                BrokerOwnedProcess::Guard(generation),
                &mut generations,
                &mut processes,
                &mut logging,
            )?;
            assert!(forwarded.is_none(), "{event:?} must forward no event");
        }

        // A stopped guard remains owned and waitable, so its ownership entry
        // must survive for the eventual terminal event to correlate against.
        assert_eq!(
            processes.get(&process).copied(),
            Some(BrokerOwnedProcess::Guard(generation))
        );
        Ok(())
    }

    /// A guard that exits still fails containment closed.
    #[test]
    fn an_exited_guard_reports_containment_failure() -> Result<(), Box<dyn Error>> {
        let generation = test_generation()?;
        let process = guard_process()?;
        let mut generations = BTreeMap::new();
        let mut processes = BTreeMap::new();
        let mut logging = empty_logging()?;

        let forwarded = correlate_owned_event(
            ChildEvent::Exited {
                pid: process,
                code: 0,
            },
            BrokerOwnedProcess::Guard(generation),
            &mut generations,
            &mut processes,
            &mut logging,
        )?;
        assert_eq!(
            forwarded,
            Some(BrokerEvent::ContainmentFailed { generation })
        );
        Ok(())
    }

    /// A signalled guard is terminal too, and must not be mistaken for a stop.
    #[test]
    fn a_signalled_guard_reports_containment_failure() -> Result<(), Box<dyn Error>> {
        let generation = test_generation()?;
        let process = guard_process()?;
        let mut generations = BTreeMap::new();
        let mut processes = BTreeMap::new();
        let mut logging = empty_logging()?;

        let forwarded = correlate_owned_event(
            ChildEvent::Signaled {
                pid: process,
                signal: 9,
            },
            BrokerOwnedProcess::Guard(generation),
            &mut generations,
            &mut processes,
            &mut logging,
        )?;
        assert_eq!(
            forwarded,
            Some(BrokerEvent::ContainmentFailed { generation })
        );
        Ok(())
    }
}
