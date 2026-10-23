//! Supervisor child-policy hygiene and ownership-checked PID delivery.
//!
//! Startup explicitly corrects inherited auto-reaping before any children or
//! runtime threads exist. Cleanup rechecks policy and performs an exact-child
//! nonblocking wait: a terminal event is returned to its owner, never followed
//! by a signal. A nonterminal result leaves the PID reserved until collection.
//! The caller must serialize policy and child collection across this operation.

use std::io;

use super::{ChildEvent, ProcessId, ProcessSignal, SignalTarget, reap_event, signal};

/// Observe whether exited children remain waitable, without changing policy.
/// This snapshot is neither child ownership nor synchronization.
///
/// # Errors
///
/// Returns an OS error querying SIGCHLD; interrupted queries are retried.
pub fn children_are_waitable() -> io::Result<bool> {
    fork::children_are_waitable()
}

/// Correct inherited auto-reaping before children, threads or dependent handlers exist.
/// Returns true only when SIGCHLD was restored to default with an empty handler
/// mask and no flags; valid installed handlers and the process mask are unchanged.
/// This deliberately changes process-wide state, not executable-entrypoint policy.
///
/// # Errors
///
/// Returns an OS error querying or correcting the disposition.
pub fn ensure_waitable_children() -> io::Result<bool> {
    fork::ensure_waitable_children()
}

/// Establish waitable children before preparation, daemonization or broker fork.
/// Idempotence lets both entry boundaries share this policy without duplicate
/// warnings; failure prevents process creation at that boundary.
pub(crate) fn prepare_child_policy() -> io::Result<()> {
    if ensure_waitable_children()? {
        tracing::warn!(
            "restored inherited SIGCHLD auto-reaping policy to retain waitable children"
        );
    }
    Ok(())
}

/// Signal an exact owned child, or return its consumed terminal event instead.
///
/// A terminal reap never authorizes delivery. A nonterminal observation leaves
/// the PID pinned even if the child exits before delivery. The caller must own
/// collection and prevent handlers or other threads changing policy or reaping
/// this child until the operation finishes. Nonterminal stop events may be consumed.
///
/// # Errors
///
/// Rejects auto-reaping policy with `InvalidInput`, lost ownership with `ECHILD`,
/// and any other policy, wait or signal error without retrying an unowned PID.
pub fn signal_child(
    process: ProcessId,
    requested: ProcessSignal,
) -> io::Result<Option<ChildEvent>> {
    if !children_are_waitable()? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "child signal requires waitable owner policy",
        ));
    }
    if let Some(event) = reap_event(process)?
        && event.is_terminal()
    {
        return Ok(Some(event));
    }
    signal(SignalTarget::Process(process), requested)?;
    Ok(None)
}
