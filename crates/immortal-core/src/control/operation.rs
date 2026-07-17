//! Request-intent vocabulary and public control protocol limits.
//!
//! These enums are the stable typed form of operator intent before a request is
//! serialized or applied to a supervisor. The numeric codes remain private to
//! the `message` codec so callers use names and typed generation conditions
//! rather than depending on wire values.

use std::time::Duration;

use crate::supervisor::Generation;

/// Current control-protocol version.
pub const PROTOCOL_VERSION: u8 = 1;
/// Hard upper bound for any request or response frame.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Maximum idle time for one control-frame read or write.
pub const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Default number of concurrently handled control connections.
pub const DEFAULT_MAX_CONTROL_CLIENTS: usize = 32;

/// Control operation accepted by a supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Inspect status without mutation.
    Status,
    /// Set persistent desired state to Up.
    Start,
    /// Stop the process group and remain supervised Down.
    Stop,
    /// Stop, reap, and create a new generation.
    Restart,
    /// Run one generation and remain Down afterward.
    Once,
    /// Leave the service running and exit its supervisor.
    Exit,
    /// Stop the process group and exit its supervisor.
    Halt,
    /// Deliver the signal carried in `Request::signal`.
    Signal,
}

impl Operation {
    pub(super) const fn code(self) -> u8 {
        match self {
            Self::Status => 1,
            Self::Start => 2,
            Self::Stop => 3,
            Self::Restart => 4,
            Self::Once => 5,
            Self::Exit => 6,
            Self::Halt => 7,
            Self::Signal => 8,
        }
    }

    pub(super) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Status),
            2 => Some(Self::Start),
            3 => Some(Self::Stop),
            4 => Some(Self::Restart),
            5 => Some(Self::Once),
            6 => Some(Self::Exit),
            7 => Some(Self::Halt),
            8 => Some(Self::Signal),
            _ => None,
        }
    }
}

/// Explicit target for raw signal delivery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SignalScope {
    /// Signal only the main child.
    #[default]
    Main,
    /// Signal the entire owned process group.
    Group,
}

impl SignalScope {
    pub(super) const fn code(self) -> u8 {
        match self {
            Self::Main => 1,
            Self::Group => 2,
        }
    }

    pub(super) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Main),
            2 => Some(Self::Group),
            _ => None,
        }
    }
}

/// Portable signal vocabulary exposed by `immortalctl`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// `SIGUSR1` (`-1`).
    User1,
    /// `SIGUSR2` (`-2`).
    User2,
    /// `SIGALRM` (`-a`).
    Alarm,
    /// `SIGCONT` (`-c`).
    Continue,
    /// `SIGHUP` (`-h`).
    Hangup,
    /// `SIGINT` (`-i`).
    Interrupt,
    /// `SIGKILL` (`-k`).
    Kill,
    /// `SIGTTIN` (`-in`).
    TerminalInput,
    /// `SIGTTOU` (`-ou`).
    TerminalOutput,
    /// `SIGQUIT` (`-q`).
    Quit,
    /// `SIGSTOP` (`-s`).
    Stop,
    /// `SIGTERM` (`-t`).
    Terminate,
    /// `SIGWINCH` (`-w`).
    WindowChange,
}

/// Optimistic-concurrency condition attached to a request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GenerationMatch {
    /// Do not compare child generation. Reserved for read-only status requests.
    #[default]
    Any,
    /// Require that the supervisor currently has no child generation.
    NoChild,
    /// Require one exact live or transitioning generation.
    Exact(Generation),
}

impl GenerationMatch {
    const NO_CHILD_SENTINEL: u64 = u64::MAX;

    pub(super) const fn encode(self) -> u64 {
        match self {
            Self::Any => 0,
            Self::NoChild => Self::NO_CHILD_SENTINEL,
            Self::Exact(generation) => generation.get(),
        }
    }

    pub(super) const fn decode(value: u64) -> Self {
        match value {
            0 => Self::Any,
            Self::NO_CHILD_SENTINEL => Self::NoChild,
            generation => Self::Exact(Generation::from_protocol(generation)),
        }
    }

    /// Whether this condition matches the supervisor's current generation.
    #[must_use]
    pub const fn matches(self, current: Option<Generation>) -> bool {
        match (self, current) {
            (Self::Any, _) | (Self::NoChild, None) => true,
            (Self::Exact(expected), Some(actual)) => expected.get() == actual.get(),
            (Self::NoChild, Some(_)) | (Self::Exact(_), None) => false,
        }
    }
}

impl Signal {
    /// Parse the stable case-insensitive command name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("usr1") {
            Some(Self::User1)
        } else if name.eq_ignore_ascii_case("usr2") {
            Some(Self::User2)
        } else if name.eq_ignore_ascii_case("alrm") {
            Some(Self::Alarm)
        } else if name.eq_ignore_ascii_case("cont") {
            Some(Self::Continue)
        } else if name.eq_ignore_ascii_case("hup") {
            Some(Self::Hangup)
        } else if name.eq_ignore_ascii_case("int") {
            Some(Self::Interrupt)
        } else if name.eq_ignore_ascii_case("kill") {
            Some(Self::Kill)
        } else if name.eq_ignore_ascii_case("ttin") {
            Some(Self::TerminalInput)
        } else if name.eq_ignore_ascii_case("ttou") {
            Some(Self::TerminalOutput)
        } else if name.eq_ignore_ascii_case("quit") {
            Some(Self::Quit)
        } else if name.eq_ignore_ascii_case("stop") {
            Some(Self::Stop)
        } else if name.eq_ignore_ascii_case("term") {
            Some(Self::Terminate)
        } else if name.eq_ignore_ascii_case("winch") {
            Some(Self::WindowChange)
        } else {
            None
        }
    }

    /// Stable lowercase name used in diagnostics and structured output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::User1 => "usr1",
            Self::User2 => "usr2",
            Self::Alarm => "alrm",
            Self::Continue => "cont",
            Self::Hangup => "hup",
            Self::Interrupt => "int",
            Self::Kill => "kill",
            Self::TerminalInput => "ttin",
            Self::TerminalOutput => "ttou",
            Self::Quit => "quit",
            Self::Stop => "stop",
            Self::Terminate => "term",
            Self::WindowChange => "winch",
        }
    }

    pub(super) const fn code(self) -> u8 {
        match self {
            Self::User1 => 1,
            Self::User2 => 2,
            Self::Alarm => 3,
            Self::Continue => 4,
            Self::Hangup => 5,
            Self::Interrupt => 6,
            Self::Kill => 7,
            Self::TerminalInput => 8,
            Self::TerminalOutput => 9,
            Self::Quit => 10,
            Self::Stop => 11,
            Self::Terminate => 12,
            Self::WindowChange => 13,
        }
    }

    pub(super) const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::User1),
            2 => Some(Self::User2),
            3 => Some(Self::Alarm),
            4 => Some(Self::Continue),
            5 => Some(Self::Hangup),
            6 => Some(Self::Interrupt),
            7 => Some(Self::Kill),
            8 => Some(Self::TerminalInput),
            9 => Some(Self::TerminalOutput),
            10 => Some(Self::Quit),
            11 => Some(Self::Stop),
            12 => Some(Self::Terminate),
            13 => Some(Self::WindowChange),
            _ => None,
        }
    }
}
