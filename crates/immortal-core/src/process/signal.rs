//! Portable signal vocabulary and checked delivery to an owned target.
//!
//! `ProcessSignal` is Immortal's stable signal name, independent of the raw
//! `libc`/`fork` constant and of the one-byte code the broker protocol writes
//! on the wire. `signal` is the single checked entry point that turns a typed
//! `SignalTarget` into the correct `fork` process or process-group delivery
//! call; no other code in this crate calls `fork::signal_process` directly.

use std::io;

use super::identity::SignalTarget;

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
    const fn into_fork(self) -> fork::Signal {
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

#[cfg(test)]
mod tests {
    use super::ProcessSignal;

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
