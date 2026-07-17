//! Non-blocking and blocking drains of the canonical `fork` wait boundary.
//!
//! `reap_any_event` is the broker's `SIGCHLD`-driven, non-blocking drain: it
//! must be called repeatedly until it returns `Ok(None)` so a coalesced signal
//! never leaves a terminated child unreaped. `wait_for_event` is the one
//! blocking call in this crate, used only to reap the broker itself after the
//! Tokio supervisor runtime has shut down. Both translate `fork`'s wait event
//! into Immortal's portable [`ChildEvent`], rejecting any signal number that
//! cannot fit the crate's stable representation instead of silently
//! truncating it.

use std::io;

use super::identity::ProcessId;

/// One state change drained from the canonical `fork` wait boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ChildEvent {
    /// The child exited normally with an eight-bit status.
    Exited { pid: ProcessId, code: u8 },
    /// The child was terminated by a signal.
    Signaled { pid: ProcessId, signal: u8 },
    /// The child was stopped but remains owned and waitable.
    Stopped { pid: ProcessId, signal: u8 },
    /// A stopped child resumed execution.
    Continued { pid: ProcessId },
}

impl ChildEvent {
    /// Return the child associated with this event.
    #[must_use]
    pub const fn pid(self) -> ProcessId {
        match self {
            Self::Exited { pid, .. }
            | Self::Signaled { pid, .. }
            | Self::Stopped { pid, .. }
            | Self::Continued { pid } => pid,
        }
    }

    /// Whether this event permanently reaped the child.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Exited { .. } | Self::Signaled { .. })
    }

    /// Convert a terminal event into restart-policy input.
    #[must_use]
    pub const fn terminal_result(self) -> Option<crate::supervisor::ChildResult> {
        match self {
            Self::Exited { code, .. } => Some(crate::supervisor::ChildResult::Exited(code)),
            Self::Signaled { signal, .. } => Some(crate::supervisor::ChildResult::Signaled(signal)),
            Self::Stopped { .. } | Self::Continued { .. } => None,
        }
    }
}

/// Drain one pending child state change without blocking.
///
/// The process broker must call this repeatedly after a coalesced `SIGCHLD`
/// until it returns `Ok(None)` or the OS reports that no children remain.
///
/// # Errors
///
/// Returns an operating-system wait error or invalid event data from `fork`.
pub fn reap_any_event() -> io::Result<Option<ChildEvent>> {
    fork::wait_any_event_nohang()?
        .map(child_event_from_fork)
        .transpose()
}

/// Wait for one state change from an exact direct child.
///
/// This blocking operation is used to reap the broker itself after the Tokio
/// supervisor runtime has shut down.
///
/// # Errors
///
/// Returns an operating-system wait error or invalid event data from `fork`.
pub fn wait_for_event(process: ProcessId) -> io::Result<ChildEvent> {
    let process = fork::ProcessId::try_from(process.get())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
    child_event_from_fork(fork::wait_event(process)?)
}

fn child_event_from_fork(event: fork::ChildEvent) -> io::Result<ChildEvent> {
    let pid = ProcessId::try_from(event.pid().get())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    match event {
        fork::ChildEvent::Exited { code, .. } => Ok(ChildEvent::Exited { pid, code }),
        fork::ChildEvent::Signalled { signal, .. } => Ok(ChildEvent::Signaled {
            pid,
            signal: signal_number(signal)?,
        }),
        fork::ChildEvent::Stopped { signal, .. } => Ok(ChildEvent::Stopped {
            pid,
            signal: signal_number(signal)?,
        }),
        fork::ChildEvent::Continued { .. } => Ok(ChildEvent::Continued { pid }),
    }
}

fn signal_number(signal: fork::Signal) -> io::Result<u8> {
    u8::try_from(signal.get()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "signal number is outside Immortal's portable representation",
        )
    })
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;

    use super::{ChildEvent, ProcessId, child_event_from_fork};

    #[test]
    fn canonical_fork_events_become_immortal_events() -> Result<(), Box<dyn Error>> {
        let fork_pid =
            fork::ProcessId::new(123).ok_or_else(|| io::Error::other("invalid test process ID"))?;
        let pid = ProcessId::new(123).ok_or_else(|| io::Error::other("invalid test process ID"))?;
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Exited {
                pid: fork_pid,
                code: 42,
            })?,
            ChildEvent::Exited { pid, code: 42 }
        );
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Signalled {
                pid: fork_pid,
                signal: fork::Signal::TERM,
            })?,
            ChildEvent::Signaled {
                pid,
                signal: u8::try_from(fork::Signal::TERM.get())?,
            }
        );
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Stopped {
                pid: fork_pid,
                signal: fork::Signal::STOP,
            })?,
            ChildEvent::Stopped {
                pid,
                signal: u8::try_from(fork::Signal::STOP.get())?,
            }
        );
        assert_eq!(
            child_event_from_fork(fork::ChildEvent::Continued { pid: fork_pid })?,
            ChildEvent::Continued { pid }
        );
        Ok(())
    }
}
