//! Release a finished generation's group guard without ending the broker over
//! a slow or vanished helper.
//!
//! A generation that is over has its guard disarmed, so the helper exits
//! without killing the group and releases the pin on the group ID. `fork`
//! bounds that release. A helper that does not exit in time is killed and
//! reaped, and the call reports `TimedOut` although the pin is released just as
//! a disarm would release it; the group is not signalled. A helper that died
//! earlier reports a socket error instead. The owner socket is closed on every
//! path, so a helper that is still alive sees end of file and kills its pinned
//! group: containment fails closed. Callers release only a group they already
//! retired or proved empty, so none of these outcomes leaves a member
//! uncontained, and none is a reason to end supervision. A slow helper is a
//! slow machine, and ending the broker would kill every other service it owns.
//! Only `InvalidInput`, which reports a violated waitable-child policy, means
//! the broker's ownership model is broken; it stays fatal.
//!
//! What a failed release can still leave behind is the helper process. An exact
//! non-blocking probe settles that: a helper that is gone needs nothing more,
//! while one that is still an unreaped child is recorded as a released guard so
//! its later reap is absorbed instead of surfacing as an unowned child, which is
//! fatal where the broker does not hold the subreaper role. No fork happens
//! between the release and the probe, so the probed PID cannot have been reused.

use std::collections::BTreeMap;
use std::io;

use crate::supervisor::Generation;

use super::super::error::ProcessBrokerError;
use super::super::spawn::GROUP_GUARD_CLEANUP_TIMEOUT;
use super::super::state::BrokerOwnedProcess;
use super::super::{ChildEvent, ProcessGroupGuard, ProcessId, reap_event};

/// Whether a guard release completed as requested.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in super::super) enum GuardRelease {
    /// The helper exited on request, or there was no guard to release.
    Released,
    /// The release failed without breaking ownership; the group stays as the
    /// caller left it, and a helper still exiting is collected later.
    Failed,
}

/// Release `guard`, whose helper is `helper`, for `generation`.
///
/// # Errors
///
/// Returns a waitable-child policy violation or a failed helper probe.
pub(super) fn release_guard(
    generation: Generation,
    guard: ProcessGroupGuard,
    helper: ProcessId,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<GuardRelease, ProcessBrokerError> {
    release_guard_with(
        generation,
        helper,
        processes,
        || guard.disarm(GROUP_GUARD_CLEANUP_TIMEOUT),
        reap_event,
    )
}

/// [`release_guard`] with an injectable disarm and helper probe.
///
/// # Errors
///
/// Returns `InvalidInput` from the disarm, or any probe error other than
/// `ECHILD`, which proves the helper is no longer this broker's child.
pub(super) fn release_guard_with<D, P>(
    generation: Generation,
    helper: ProcessId,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    disarm: D,
    probe: P,
) -> Result<GuardRelease, ProcessBrokerError>
where
    D: FnOnce() -> io::Result<()>,
    P: FnOnce(ProcessId) -> io::Result<Option<ChildEvent>>,
{
    let Err(error) = disarm() else {
        return Ok(GuardRelease::Released);
    };
    if error.kind() == io::ErrorKind::InvalidInput {
        return Err(error.into());
    }
    let helper_exiting = collect_released_helper_with(generation, helper, processes, probe)?;
    crate::diagnostic::report(format_args!(
        "immortal process broker: releasing the group guard of generation {} failed: {error}; {}",
        generation.get(),
        if helper_exiting {
            "its helper is still exiting and will be collected"
        } else {
            "its helper is gone"
        }
    ));
    Ok(GuardRelease::Failed)
}

/// Track the helper of a guard closed fail-closed until it is collected.
///
/// Dropping an armed guard closes its owner socket, so the helper kills the
/// group, and then waits for the helper and kills it at its deadline. A
/// helper that still is not collected afterwards is recorded like one whose
/// release failed, so its later reap is absorbed instead of surfacing as an
/// unowned child, which is fatal where the broker does not hold the subreaper
/// role.
///
/// # Errors
///
/// Returns a probe error other than `ECHILD`.
pub(in super::super) fn collect_closed_helper(
    generation: Generation,
    helper: ProcessId,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<(), ProcessBrokerError> {
    if collect_released_helper_with(generation, helper, processes, reap_event)? {
        crate::diagnostic::report(format_args!(
            "immortal process broker: the closed group guard of generation {} left its helper \
             still exiting; it will be collected",
            generation.get()
        ));
    }
    Ok(())
}

/// Probe `helper` exactly and record it while it is still an unreaped child.
///
/// Returns whether the helper is still exiting. No fork happens between the
/// release and the probe, so the probed PID cannot have been reused.
///
/// # Errors
///
/// Returns a probe error other than `ECHILD`, which proves the helper is no
/// longer this broker's child.
pub(super) fn collect_released_helper_with<P>(
    generation: Generation,
    helper: ProcessId,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    probe: P,
) -> Result<bool, ProcessBrokerError>
where
    P: FnOnce(ProcessId) -> io::Result<Option<ChildEvent>>,
{
    let helper_exiting = match probe(helper) {
        Ok(Some(event)) => !event.is_terminal(),
        Ok(None) => true,
        Err(probe_error) if probe_error.raw_os_error() == Some(libc::ECHILD) => false,
        Err(probe_error) => return Err(probe_error.into()),
    };
    if helper_exiting {
        processes.insert(helper, BrokerOwnedProcess::ReleasedGuard(generation));
    }
    Ok(helper_exiting)
}

#[cfg(test)]
mod tests;
