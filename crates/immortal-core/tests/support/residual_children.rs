//! Proof that a broker contract left no child process behind.
//!
//! The broker is this test process's only direct child, and starting it made
//! this process the supervisor's child-subreaper where the platform offers the
//! role, so every descendant the broker abandons is reparented here. Linux and
//! FreeBSD hand a dying process's children to its reaper before the dying
//! process itself becomes waitable, so once the broker has been reaped an
//! immediate `reap_any_event` must report `ECHILD`: a leaked zombie surfaces as
//! a reaped event and a still-running leftover as an idle poll. Without the
//! subreaper role (macOS) leftovers reparent to init and cannot be observed.
//!
//! A failed check still reaps whatever leftovers exit within a short bound, so a
//! killed member which was still dying does not outlive the test as a zombie.
//! A leftover that keeps running belongs to the scenario's own cleanup guard.

use std::{
    io, thread,
    time::{Duration, Instant},
};

use immortal_core::process::reap_any_event;

/// Bounds how long a failed check keeps reaping leftovers which are exiting.
const LEFTOVER_DRAIN: Duration = Duration::from_secs(2);
const LEFTOVER_POLL: Duration = Duration::from_millis(5);

/// Fail unless this process has no child, live or exited, left to reap.
///
/// # Errors
///
/// Returns an error naming `context` when a child remains, or the wait error.
pub fn assert_no_residual_children(context: &str) -> io::Result<()> {
    let failure = match reap_any_event() {
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Ok(()),
        Ok(Some(event)) => {
            io::Error::other(format!("{context} left an exited child behind: {event:?}"))
        }
        Ok(None) => io::Error::other(format!("{context} left a running child behind")),
        Err(error) => return Err(error),
    };
    drain_leftovers();
    Err(failure)
}

/// Reap leftovers until none remains or [`LEFTOVER_DRAIN`] passes.
fn drain_leftovers() {
    let deadline = Instant::now() + LEFTOVER_DRAIN;
    while Instant::now() < deadline {
        match reap_any_event() {
            Ok(Some(_)) => {}
            Ok(None) => thread::sleep(LEFTOVER_POLL),
            Err(_) => return,
        }
    }
}
