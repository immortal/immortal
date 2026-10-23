//! Child reaping, containment-guard disarming, and group-wide signaling.
//!
//! [`collect_child_events`]/[`forward_child_events`] drain every pending
//! `SIGCHLD`-driven wait, correlating each reaped process against its owning
//! generation or containment guard: a guard that *exits* while its generation
//! is still owned is a containment failure (group kill requires a remaining
//! verified member leader's pin), while an
//! ordinary workload exit removes bookkeeping after releasing its guard. A
//! foreground reap disarms only after an acknowledged kill or ESRCH; failed or
//! skipped kills close the guard fail-closed so remaining members are killed.
//! Waits also report job-control stops and resumes, which change no ownership.
//! Both guard-release paths reconcile the same checked process-ownership entry.
//!
//! When the broker holds the child-subreaper role, orphaned descendants of the
//! supervised subtree also reparent to it; [`collect_child_events`] reaps those
//! adopted orphans for hygiene without ever forwarding a workload event. An
//! unowned reap while the role is absent remains a fatal ownership violation.
//!
//! The broker signals a group only while something pins its numeric ID to the
//! group the broker created. Armed-group signals cross the guard helper while
//! it owns the unreaped anchor; helper loss fails delivery rather than reaching
//! a reused numeric ID, even before the broker observes its exit. The helper
//! round trip has a per-call cap, reduced to the active cleanup phase's remaining
//! budget. The runtime carries that deadline through all-child draining, so
//! foreground reaps cannot renew it. Expiry never permits a direct-signal fallback.
//! Without an armed guard, an unreaped direct child whose PID equals the PGID
//! and which is still a member, checked immediately before the signal, pins it instead
//! ([`signal_live_group`], [`kill_live_group`]). A child which left its group
//! is also signalled as a process, which its unreaped PID pins. Every group
//! kill goes through [`kill_and_retire`], which records the group in
//! [`RetiringGroups`] until it has no member left. A group the broker stops
//! owning without a pin, such as a descriptor group whose guard exits, is
//! retired without any signal. Each drain ends with a prune pass, so shutdown
//! and supervisor-loss cleanup can wait for those groups to empty instead of
//! exiting while a member remains.

use std::collections::BTreeMap;
use std::io;
use std::time::{Duration, Instant};

use tokio::io::AsyncWrite;

use crate::supervisor::Generation;

use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::retiring::RetiringGroups;
use super::spawn::GROUP_GUARD_CLEANUP_TIMEOUT;
use super::state::{BrokerGeneration, BrokerOwnedProcess, BrokerRuntimeState, LifetimeState};
use super::wire::write_event;
use super::{
    BrokerEvent, ChildEvent, ProcessGroupGuard, ProcessGroupId, ProcessId, ProcessSignal,
    SignalTarget, SpawnedProcess, current_process_group, reap_any_event, signal,
};

/// Maximum helper round trip outside cleanup phases. Inside a phase, every
/// call (including foreground-reap kills) also consumes its shared remaining
/// budget, so a sweep cannot add 500 ms per helper beyond the phase deadline.
/// Timeout is not cancellation and never authorizes direct group delivery.
const GROUP_SIGNAL_TIMEOUT: Duration = Duration::from_millis(500);

/// Deliver group targets through the armed guard, and process targets directly.
/// Missing armed ownership fails closed rather than using a numeric group ID.
fn signal_owned_target(
    target: SignalTarget,
    requested: ProcessSignal,
    armed: bool,
    guard: Option<&ProcessGroupGuard>,
    deadline: Option<Instant>,
) -> io::Result<()> {
    signal_owned_target_with(
        target,
        armed,
        || {
            with_helper_timeout(deadline, |timeout| {
                let guard = guard.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "armed group guard is missing")
                })?;
                guard.signal_group(requested, timeout)
            })
        },
        |target| signal(target, requested),
    )
}

/// Bound one exchange by both the per-call cap and the active phase's remaining
/// budget. Once expired, do not queue another signal; kill callers still retire.
fn with_helper_timeout<S>(deadline: Option<Instant>, send: S) -> io::Result<()>
where
    S: FnOnce(Duration) -> io::Result<()>,
{
    let remaining = deadline.map_or(GROUP_SIGNAL_TIMEOUT, |deadline| {
        deadline
            .saturating_duration_since(Instant::now())
            .min(GROUP_SIGNAL_TIMEOUT)
    });
    if remaining.is_zero() {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "group signal phase deadline expired",
        ));
    }
    send(remaining)
}

/// Injectable routing boundary: a failed helper never falls back to direct kill.
fn signal_owned_target_with<G, S>(
    target: SignalTarget,
    armed: bool,
    guarded: G,
    direct: S,
) -> io::Result<()>
where
    G: FnOnce() -> io::Result<()>,
    S: FnOnce(SignalTarget) -> io::Result<()>,
{
    if armed && matches!(target, SignalTarget::Group(_)) {
        guarded()
    } else {
        direct(target)
    }
}

/// Drain every pending reap and forward the resulting workload events.
///
/// # Errors
///
/// Returns a reap, ownership, or event-write failure.
pub(super) async fn forward_child_events<W>(
    writer: &mut W,
    state: &mut BrokerRuntimeState,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    for event in collect_child_events(state)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

/// Drain every pending reap, then release retired groups which have drained.
///
/// # Errors
///
/// Returns a reap or ownership failure.
pub(super) fn collect_child_events(
    state: &mut BrokerRuntimeState,
) -> Result<Vec<BrokerEvent>, ProcessBrokerError> {
    let mut events = Vec::new();
    while let Some(event) = next_reaped_event()? {
        let process = event.pid();
        let Some(owned) = state.processes.get(&process).copied() else {
            reap_adopted_orphan(process, event, state.subreaper_active)?;
            continue;
        };
        if event.is_terminal() {
            state.processes.remove(&process);
        }
        if let Some(event) = correlate_owned_event(
            event,
            owned,
            &mut state.generations,
            &mut state.processes,
            &mut state.logging,
            &mut state.retiring,
            state.signal_deadline,
        )? {
            events.push(event);
        }
    }
    state.retiring.prune();
    Ok(events)
}

/// Kill a whole process group and remember it until it has drained.
///
/// A group already retiring was killed before and is never signalled again,
/// because its ID may have been reused once it emptied; that case reports
/// success. `ESRCH` means the group already has no member, so nothing is
/// retired. Any other outcome, including a failed delivery, keeps the group
/// retiring, so a shutdown waits for it and fails closed if it never drains.
///
/// # Errors
///
/// Returns the delivery error, which callers that only need the group gone
/// may ignore.
fn kill_and_retire(
    group: ProcessGroupId,
    guard: &ProcessGroupGuard,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) -> io::Result<()> {
    kill_and_retire_with(group, retiring, |group| {
        signal_owned_target(
            SignalTarget::Group(group),
            ProcessSignal::Kill,
            true,
            Some(guard),
            deadline,
        )
    })
}

/// [`kill_and_retire`] with an injectable delivery, so tests can observe it.
fn kill_and_retire_with<S>(
    group: ProcessGroupId,
    retiring: &mut RetiringGroups,
    send: S,
) -> io::Result<()>
where
    S: FnOnce(ProcessGroupId) -> io::Result<()>,
{
    if retiring.contains(group) {
        return Ok(());
    }
    let delivered = send(group);
    if !matches!(&delivered, Err(error) if error.raw_os_error() == Some(libc::ESRCH)) {
        retiring.retire(group);
    }
    delivered
}

/// Only an acknowledged foreground kill or ESRCH permits intentional disarm.
/// A retiring-set no-op is not delivery evidence: a previous kill may have
/// failed. Leave that guard fail-closed without sending another group request.
fn foreground_kill_allows_disarm_with<K>(already_retiring: bool, kill: K) -> bool
where
    K: FnOnce() -> io::Result<()>,
{
    if already_retiring {
        return false;
    }
    match kill() {
        Ok(()) => true,
        Err(error) => error.raw_os_error() == Some(libc::ESRCH),
    }
}

/// Translate one reaped event for a process this broker owns.
///
/// Returns the event to forward, or `None` when the state change carries no
/// ownership consequence. Reconciles generation bookkeeping and containment as
/// a side effect: a foreground exit disarms only after an acknowledged kill or
/// ESRCH, otherwise it closes the guard fail-closed. Tracked completion still
/// disarms intentionally. Every killed group stays retired until it drains.
fn correlate_owned_event(
    event: ChildEvent,
    owned: BrokerOwnedProcess,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) -> Result<Option<BrokerEvent>, ProcessBrokerError> {
    match owned {
        BrokerOwnedProcess::Generation(generation) => {
            if event.is_terminal()
                && let Some(entry) = generations.get(&generation)
            {
                let (group, armed, lifetime) = (entry.group, entry.guard.is_some(), entry.lifetime);
                let mut fail_closed = false;
                let remove_generation = if lifetime == LifetimeState::Foreground {
                    // Only the helper can safely deliver after the child reap:
                    // an unobserved helper death may already have lost the pin.
                    if let Some(guard) = entry.guard.as_ref() {
                        fail_closed =
                            !foreground_kill_allows_disarm_with(retiring.contains(group), || {
                                kill_and_retire(group, guard, retiring, deadline)
                            });
                    }
                    true
                } else {
                    if let Some(entry) = generations.get_mut(&generation) {
                        entry.child = None;
                    }
                    let completed =
                        matches!(lifetime, LifetimeState::Closed | LifetimeState::Failed);
                    if completed {
                        retire_completed_group(group, armed, retiring);
                    }
                    completed
                };
                if remove_generation {
                    if fail_closed {
                        fail_closed_generation_guard(generation, generations, processes)?;
                    } else {
                        disarm_generation_guard(generation, generations, processes)?;
                    }
                    generations.remove(&generation);
                    logging.child_reaped(generation);
                }
            }
            Ok(Some(BrokerEvent::Child { generation, event }))
        }
        // Only a guard that actually exited has stopped containing the group.
        // Waits also report job-control stops and resumes, and treating those
        // as containment failure would kill a healthy workload the moment its
        // guard was suspended. An exited guard no longer keeps its anchor
        // unreaped, so a descriptor group whose launcher is already gone has
        // no pin left and is awaited rather than killed.
        BrokerOwnedProcess::Guard(generation) if event.is_terminal() => {
            let target = generations.get_mut(&generation).map(|entry| {
                drop(entry.guard.take());
                (entry.group, entry.child)
            });
            if let Some((group, child)) = target {
                // Only an unreaped member leader (PID == PGID) can pin it now.
                // A nonleader can leave after a membership observation.
                let pin = GroupPin {
                    armed: false,
                    reused: pinned_by_another(generations, generation, group),
                };
                match child {
                    Some(child) => {
                        let _ =
                            kill_live_group(child.process(), group, pin, None, retiring, deadline);
                    }
                    None if !pin.reused => retiring.retire(group),
                    None => {}
                }
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
    if let Some(guard) = take_generation_guard(generation, generations, processes)? {
        guard.disarm(GROUP_GUARD_CLEANUP_TIMEOUT)?;
    }
    Ok(())
}

/// Close the owner after a failed/skipped foreground kill, preserving containment.
/// Disarming here would release the anchor while unsignalled members still live.
/// Drop resumes a stopped helper and gives EOF cleanup three seconds, then kills
/// the helper; its final reap still depends on kernel progress. The retired group
/// remains tracked. Identity/ownership mismatches fail as they do during disarm.
fn fail_closed_generation_guard(
    generation: Generation,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<(), ProcessBrokerError> {
    drop(take_generation_guard(generation, generations, processes)?);
    Ok(())
}

/// Transfer the guard only after reconciling its process-ownership record.
/// Both intentional disarm and fail-closed Drop use this identical check;
/// absence is a no-op, while an inconsistent owner remains a broker error.
fn take_generation_guard(
    generation: Generation,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<Option<ProcessGroupGuard>, ProcessBrokerError> {
    let Some(guard) = generations
        .get_mut(&generation)
        .and_then(|entry| entry.guard.take())
    else {
        return Ok(None);
    };
    let process = guard.process()?;
    if processes.remove(&process) != Some(BrokerOwnedProcess::Guard(generation)) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "process-group guard ownership is inconsistent",
        )
        .into());
    }
    Ok(Some(guard))
}

/// Await the group of a tracked generation that has just completed.
///
/// Completion means the launcher was reaped and every holder closed the
/// lifetime descriptor, so the generation is over and its group is never
/// signalled again. A member which closed the descriptor may still be running
/// or exiting, so the group is retired for shutdown to await; the guard is
/// disarmed right after, which releases its pin. A guard which already exited
/// killed and retired the group then; its ID may since have drained and been
/// reused, so it is not retired again.
pub(super) fn retire_completed_group(
    group: ProcessGroupId,
    guard_armed: bool,
    retiring: &mut RetiringGroups,
) {
    if guard_armed {
        retiring.retire(group);
    }
}

/// Whether an unreaped member leader reserves this numeric group ID.
///
/// A live nonleader can leave after getpgid. A leader's unreaped PID, equal to
/// the PGID, still prevents reuse even if it moves after the membership query.
fn pins_group<Q>(process: ProcessId, group: ProcessGroupId, query: Q) -> bool
where
    Q: FnOnce(ProcessId) -> io::Result<ProcessGroupId>,
{
    child_pins_group(process, group, is_group_member(process, group, query))
}

/// Exclusive unreaped-child ownership reserves its PID, not arbitrary PGIDs.
const fn child_pins_group(process: ProcessId, group: ProcessGroupId, member: bool) -> bool {
    member && process.get() == group.get()
}

/// Membership alone can change immediately after this observation. It is used
/// to route the child's signal, never as authority for an unarmed group kill.
fn is_group_member<Q>(process: ProcessId, group: ProcessGroupId, query: Q) -> bool
where
    Q: FnOnce(ProcessId) -> io::Result<ProcessGroupId>,
{
    matches!(query(process), Ok(current) if current == group)
}

/// Whether a live generation other than `owner` pins `group` as its own.
///
/// A group's ID is free for a new generation only once every member of the old
/// group has gone, so when `owner` no longer pins its group and another
/// generation's armed guard, or a member leader, pins a group with that ID, the
/// ID names the other generation's live group. A retire or kill made on
/// `owner`'s behalf must then leave it alone, or that generation could never be
/// stopped. Only generations recording the same ID are examined, so the cost
/// is at most one `getpgid` per such generation, which is normally none.
pub(super) fn pinned_by_another(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    owner: Generation,
    group: ProcessGroupId,
) -> bool {
    let records = generations.iter().map(|(generation, entry)| GroupRecord {
        generation: *generation,
        group: entry.group,
        armed: entry.guard.is_some(),
        child: entry.child,
    });
    pinned_by_another_with(records, owner, group, current_process_group)
}

/// The facts [`pinned_by_another`] reads from one generation.
#[derive(Clone, Copy, Debug)]
struct GroupRecord {
    generation: Generation,
    group: ProcessGroupId,
    armed: bool,
    child: Option<SpawnedProcess>,
}

fn pinned_by_another_with<Q>(
    records: impl IntoIterator<Item = GroupRecord>,
    owner: Generation,
    group: ProcessGroupId,
    mut query: Q,
) -> bool
where
    Q: FnMut(ProcessId) -> io::Result<ProcessGroupId>,
{
    records.into_iter().any(|record| {
        record.generation != owner
            && record.group == group
            && (record.armed
                || record
                    .child
                    .is_some_and(|child| pins_group(child.process(), group, &mut query)))
    })
}

/// What pins a live generation's group, decided before signalling it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct GroupPin {
    /// The generation's guard is armed: delivery must go through its helper.
    pub(super) armed: bool,
    /// Another live generation now pins the ID ([`pinned_by_another`]).
    pub(super) reused: bool,
}

/// Send `requested` to a live generation's group, and to its child if it left.
///
/// The group is signalled while the armed guard's anchor or an unreaped
/// member leader pins it, and never once it is retiring. A child which left its
/// group is signalled as a process too, which its unreaped PID pins. Without any group
/// pin only the child is signalled, and the group it left is retired without
/// a signal unless another generation now pins that ID.
///
/// # Errors
///
/// With a persistent group pin, returns the group delivery error while the
/// child is a member, and the child's own delivery error once it left: the group signal then only reaches
/// members left behind, and on macOS a group whose only member is the anchor
/// answers `EPERM`. Without a persistent pin, only the child's delivery result
/// is returned. A skipped retiring group reports success.
pub(super) fn signal_live_group(
    process: ProcessId,
    group: ProcessGroupId,
    requested: ProcessSignal,
    pin: GroupPin,
    guard: Option<&ProcessGroupGuard>,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) -> io::Result<()> {
    signal_live_group_with(
        process,
        group,
        pin,
        retiring,
        current_process_group,
        |target| signal_owned_target(target, requested, pin.armed, guard, deadline),
    )
}

fn signal_live_group_with<Q, S>(
    process: ProcessId,
    group: ProcessGroupId,
    pin: GroupPin,
    retiring: &mut RetiringGroups,
    query: Q,
    mut send: S,
) -> io::Result<()>
where
    Q: FnOnce(ProcessId) -> io::Result<ProcessGroupId>,
    S: FnMut(SignalTarget) -> io::Result<()>,
{
    let member = is_group_member(process, group, query);
    if !(pin.armed || child_pins_group(process, group, member)) {
        if !pin.reused {
            retiring.retire(group);
        }
        return send(SignalTarget::Process(process));
    }
    let delivered = if retiring.contains(group) {
        Ok(())
    } else {
        send(SignalTarget::Group(group))
    };
    if member {
        delivered
    } else {
        send(SignalTarget::Process(process))
    }
}

/// Kill a live generation's group and retire it, and its child if it left.
///
/// The group is killed while the armed guard's anchor or an unreaped member
/// leader pins it, through [`kill_and_retire`], which retires it even when
/// delivery fails. A child which left its group is killed as a process too. Without any
/// group pin only the child is killed, and the group it was spawned into is
/// retired without a signal unless another generation now pins that ID.
///
/// # Errors
///
/// Returns delivery errors as [`signal_live_group`] does.
pub(super) fn kill_live_group(
    process: ProcessId,
    group: ProcessGroupId,
    pin: GroupPin,
    guard: Option<&ProcessGroupGuard>,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) -> io::Result<()> {
    kill_live_group_with(
        process,
        group,
        pin,
        retiring,
        current_process_group,
        |target| signal_owned_target(target, ProcessSignal::Kill, pin.armed, guard, deadline),
    )
}

fn kill_live_group_with<Q, S>(
    process: ProcessId,
    group: ProcessGroupId,
    pin: GroupPin,
    retiring: &mut RetiringGroups,
    query: Q,
    mut send: S,
) -> io::Result<()>
where
    Q: FnOnce(ProcessId) -> io::Result<ProcessGroupId>,
    S: FnMut(SignalTarget) -> io::Result<()>,
{
    let member = is_group_member(process, group, query);
    if !(pin.armed || child_pins_group(process, group, member)) {
        if !pin.reused {
            retiring.retire(group);
        }
        return send(SignalTarget::Process(process));
    }
    let killed = kill_and_retire_with(group, retiring, |group| send(SignalTarget::Group(group)));
    if member {
        killed
    } else {
        send(SignalTarget::Process(process))
    }
}

/// The pin facts for `generation`'s group, from its entry and its siblings.
pub(super) fn group_pin(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    generation: Generation,
    entry: &BrokerGeneration,
) -> GroupPin {
    GroupPin {
        armed: entry.guard.is_some(),
        reused: pinned_by_another(generations, generation, entry.group),
    }
}

/// Ask every live generation to terminate, as [`signal_live_group`] targets it.
///
/// Delivery failures are ignored: a target which already exited is the
/// outcome the caller wanted.
pub(super) fn terminate_every_group(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) {
    for (generation, entry, child) in live_children(generations) {
        let pin = group_pin(generations, generation, entry);
        let _ = signal_live_group(
            child.process(),
            child.group(),
            ProcessSignal::Terminate,
            pin,
            entry.guard.as_ref(),
            retiring,
            deadline,
        );
    }
}

/// Kill every live generation and retire its group, as [`kill_live_group`]
/// targets it.
pub(super) fn kill_every_group(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) {
    for (generation, entry, child) in live_children(generations) {
        let pin = group_pin(generations, generation, entry);
        let _ = kill_live_group(
            child.process(),
            child.group(),
            pin,
            entry.guard.as_ref(),
            retiring,
            deadline,
        );
    }
}

/// Send `requested` to every descriptor group whose armed guard pins it.
///
/// A descriptor group's launcher was already reaped, so only the guard's
/// anchor pins the group; a group without an armed guard is never signalled. A
/// kill also retires the group. Delivery failures are ignored, as above.
pub(super) fn signal_descriptor_groups(
    generations: &BTreeMap<Generation, BrokerGeneration>,
    requested: ProcessSignal,
    retiring: &mut RetiringGroups,
    deadline: Option<Instant>,
) {
    let pinned = generations
        .values()
        .filter(|entry| entry.child.is_none())
        .filter_map(|entry| entry.guard.as_ref().map(|guard| (entry.group, guard)));
    for (group, guard) in pinned {
        if requested == ProcessSignal::Kill {
            let _ = kill_and_retire(group, guard, retiring, deadline);
        } else if !retiring.contains(group) {
            let _ = signal_owned_target(
                SignalTarget::Group(group),
                requested,
                true,
                Some(guard),
                deadline,
            );
        }
    }
}

/// Every generation whose direct child has not been reaped yet.
fn live_children(
    generations: &BTreeMap<Generation, BrokerGeneration>,
) -> impl Iterator<Item = (Generation, &BrokerGeneration, SpawnedProcess)> + '_ {
    generations
        .iter()
        .filter_map(|(generation, entry)| entry.child.map(|child| (*generation, entry, child)))
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
mod tests;
