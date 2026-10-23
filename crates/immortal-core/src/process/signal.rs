//! Portable signal vocabulary and checked delivery to an owned target.
//!
//! `ProcessSignal` is Immortal's stable signal name, independent of the raw
//! `libc`/`fork` constant and of the one-byte code the broker protocol writes
//! on the wire. `signal` is the single checked entry point that turns a typed
//! `SignalTarget` into the correct `fork` process or process-group delivery
//! call, and `process_group_exists` is the matching null-signal membership
//! probe; no other code in this crate reaches `kill(2)` through `fork`.
//! `current_process_group` reports which group a process belongs to now, so
//! the broker can tell whether a child is still a member of its group before
//! signalling it.

use std::io;

use super::identity::{ProcessGroupId, ProcessId, SignalTarget};

/// Portable signal vocabulary used by Immortal's process executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProcessSignal {
    Hangup,
    Interrupt,
    Quit,
    Kill,
    Alarm,
    Terminate,
    Stop,
    Continue,
    User1,
    User2,
    TerminalInput,
    TerminalOutput,
    WindowChange,
}

impl ProcessSignal {
    /// Map the portable signal once for both direct and guard-mediated delivery.
    pub(super) const fn into_fork(self) -> fork::Signal {
        match self {
            Self::Hangup => fork::Signal::HUP,
            Self::Interrupt => fork::Signal::INT,
            Self::Quit => fork::Signal::QUIT,
            Self::Kill => fork::Signal::KILL,
            Self::Alarm => fork::Signal::ALRM,
            Self::Terminate => fork::Signal::TERM,
            Self::Stop => fork::Signal::STOP,
            Self::Continue => fork::Signal::CONT,
            Self::User1 => fork::Signal::USR1,
            Self::User2 => fork::Signal::USR2,
            Self::TerminalInput => fork::Signal::TTIN,
            Self::TerminalOutput => fork::Signal::TTOU,
            Self::WindowChange => fork::Signal::WINCH,
        }
    }

    pub(super) const fn code(self) -> u8 {
        match self {
            Self::Hangup => 1,
            Self::Interrupt => 2,
            Self::Quit => 3,
            Self::Kill => 4,
            Self::Alarm => 5,
            Self::Terminate => 6,
            Self::Stop => 7,
            Self::Continue => 8,
            Self::User1 => 9,
            Self::User2 => 10,
            Self::TerminalInput => 11,
            Self::TerminalOutput => 12,
            Self::WindowChange => 13,
        }
    }

    pub(super) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Hangup),
            2 => Some(Self::Interrupt),
            3 => Some(Self::Quit),
            4 => Some(Self::Kill),
            5 => Some(Self::Alarm),
            6 => Some(Self::Terminate),
            7 => Some(Self::Stop),
            8 => Some(Self::Continue),
            9 => Some(Self::User1),
            10 => Some(Self::User2),
            11 => Some(Self::TerminalInput),
            12 => Some(Self::TerminalOutput),
            13 => Some(Self::WindowChange),
            _ => None,
        }
    }
}

/// Deliver one checked signal to an explicitly typed target.
///
/// # Errors
///
/// Returns the operating-system signal-delivery error from the canonical fork boundary.
pub fn signal(target: SignalTarget, signal: ProcessSignal) -> io::Result<()> {
    match target {
        SignalTarget::Process(process) => fork::signal_process(
            fork::ProcessId::try_from(process.get())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
            signal.into_fork(),
        ),
        SignalTarget::Group(group) => fork::signal_process_group(
            fork::ProcessGroupId::try_from(group.get())
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
            signal.into_fork(),
        ),
    }
}

/// Return the process group `process` belongs to now.
///
/// The broker calls this immediately before a group signal to learn whether its
/// unreaped direct child is still a member of the group it was spawned into.
/// Without an armed guard membership is insufficient: only a child's own
/// unreaped PID can reserve an equal PGID. A nonleader can leave immediately
/// after this observation. A child which moved to another group is signalled as a process either way.
///
/// # Errors
///
/// Returns the operating-system error when the process does not exist, is a
/// zombie the platform does not report (FreeBSD, macOS), or may not be
/// inspected.
pub(crate) fn current_process_group(process: ProcessId) -> io::Result<ProcessGroupId> {
    let group = fork::process_group(
        fork::ProcessId::try_from(process.get())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
    )?;
    ProcessGroupId::try_from(group.get())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Report whether any process, including an unreaped zombie, still belongs to `group`.
///
/// This is the null-signal check of the canonical fork boundary: nothing is
/// delivered, and a member the broker may not signal still counts as present.
/// The answer is a point-in-time observation. Once a group has emptied, the
/// kernel may hand its ID to an unrelated group, so `true` never proves the
/// original group still exists and must never be a reason to signal it; the
/// broker only uses it to wait, under a deadline, for groups it has already
/// killed to drain.
///
/// # Errors
///
/// Returns [`io::ErrorKind::InvalidInput`] for group 1, which `kill(2)` treats
/// as a broadcast, and any other operating-system error from the probe.
pub fn process_group_exists(group: ProcessGroupId) -> io::Result<bool> {
    fork::process_group_exists(
        fork::ProcessGroupId::try_from(group.get())
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?,
    )
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::{ProcessSignal, current_process_group, process_group_exists};
    use crate::process::{ProcessGroupId, ProcessId};

    #[test]
    fn current_process_group_reports_the_callers_group() -> Result<(), Box<dyn Error>> {
        let own = ProcessId::new(i32::try_from(std::process::id())?).ok_or("invalid own pid")?;
        assert_eq!(
            current_process_group(own)?.get(),
            fork::current_process_group_id().get()
        );
        Ok(())
    }

    #[test]
    fn process_group_exists_rejects_the_broadcast_group() -> Result<(), Box<dyn Error>> {
        let broadcast = ProcessGroupId::try_from(1)?;
        let refused = process_group_exists(broadcast)
            .err()
            .ok_or("group 1 must be refused rather than probed as a broadcast")?;
        assert_eq!(refused.kind(), io::ErrorKind::InvalidInput);
        Ok(())
    }

    #[test]
    fn every_process_signal_maps_to_a_checked_fork_signal() {
        for signal in [
            ProcessSignal::Hangup,
            ProcessSignal::Interrupt,
            ProcessSignal::Quit,
            ProcessSignal::Kill,
            ProcessSignal::Alarm,
            ProcessSignal::Terminate,
            ProcessSignal::Stop,
            ProcessSignal::Continue,
            ProcessSignal::User1,
            ProcessSignal::User2,
            ProcessSignal::TerminalInput,
            ProcessSignal::TerminalOutput,
            ProcessSignal::WindowChange,
        ] {
            assert!(signal.into_fork().get() > 0);
        }
    }
}
