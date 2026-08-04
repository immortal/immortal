//! Typed control actions and focused operation handlers.
//!
//! Dispatch constructs one exhaustive operation variant around shared control
//! inputs. The named binary routes that variant to a focused handler, while the
//! private control engine owns runtime discovery, transport, waiting, and
//! output behavior.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
    time::Duration,
};

use immortal_core::{
    control::{Operation, Signal, SignalScope, TransportError},
    exit::ExitClass,
    runtime::RuntimeRootError,
    status::ServiceState,
};

pub mod exit;
pub mod halt;
pub mod once;
pub mod restart;
pub mod signal;
pub mod start;
pub mod status;
pub mod stop;

mod control;

/// Target selected by an operator.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Target {
    /// One named service.
    Service(String),
    /// Every safely discovered service.
    All,
}

/// Stable operator output format.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    /// Human-readable fixed columns.
    Table,
    /// Machine-readable JSON array.
    Json,
}

/// Automatic runtime roots selected for discovery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeScope {
    /// Search both platform system and effective-user roots.
    All,
    /// Search only the platform system root.
    System,
    /// Search only the effective user's root.
    User,
}

/// Runtime-root selection after CLI precedence is applied.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RuntimeDiscovery {
    /// Discover documented roots for the selected scope.
    Automatic(RuntimeScope),
    /// Discover exactly one operator-provided root.
    Custom(PathBuf),
}

/// Inputs shared by every control operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlAction {
    /// Typed automatic or exact runtime-root selection.
    pub discovery: RuntimeDiscovery,
    /// Output representation.
    pub output: OutputFormat,
    /// Omit the table header.
    pub no_header: bool,
    /// Hard lifecycle completion deadline.
    pub wait_timeout: Duration,
    /// Return after request acceptance instead of polling completion.
    pub no_wait: bool,
    /// One service or the discovery set.
    pub target: Target,
    /// Explicit raw-signal target.
    pub scope: SignalScope,
    /// Raw signal, present only for [`Action::Signal`].
    pub signal: Option<Signal>,
}

/// One mutually exclusive control operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Query supervisor status.
    Status(ControlAction),
    /// Start supervised services.
    Start(ControlAction),
    /// Stop supervised services.
    Stop(ControlAction),
    /// Restart supervised services.
    Restart(ControlAction),
    /// Run supervised services once.
    Once(ControlAction),
    /// Ask supervisors to exit after stopping their services.
    Exit(ControlAction),
    /// Halt supervisors and their services.
    Halt(ControlAction),
    /// Deliver one typed signal.
    Signal(ControlAction),
}

impl Action {
    pub(crate) fn from_operation(operation: Operation, control: ControlAction) -> Self {
        match operation {
            Operation::Status => Self::Status(control),
            Operation::Start => Self::Start(control),
            Operation::Stop => Self::Stop(control),
            Operation::Restart => Self::Restart(control),
            Operation::Once => Self::Once(control),
            Operation::Exit => Self::Exit(control),
            Operation::Halt => Self::Halt(control),
            Operation::Signal => Self::Signal(control),
        }
    }
}

/// Failure while discovering or contacting supervisors.
#[derive(Debug)]
pub enum ActionError {
    /// Tokio could not create the bounded control runtime.
    RuntimeInitialization(io::Error),
    /// Runtime root is absent or unsafe.
    Runtime(RuntimeRootError),
    /// Requested service was not safely discovered.
    ServiceNotFound(String),
    /// A service name exists in more than one selected runtime scope.
    AmbiguousService(String),
    /// Combined automatic discovery exceeded its global service bound.
    ServiceLimit,
    /// Unix socket connection failed.
    Connect(io::Error),
    /// Connect deadline elapsed.
    ConnectTimeout,
    /// Lifecycle did not reach its requested terminal state before the deadline.
    LifecycleTimeout,
    /// The supervisor settled where the requested lifecycle goal is unreachable.
    LifecycleAbandoned(ServiceState),
    /// A successful status response omitted its typed payload.
    StatusUnavailable,
    /// Framed request or response failed.
    Transport(TransportError),
    /// Human or machine-readable output failed.
    Output(io::Error),
    /// JSON serialization failed.
    Json(serde_json::Error),
    /// One supervisor returned a stable non-success result.
    Remote(ExitClass),
    /// At least one supervisor rejected or failed an operation.
    PartialFailure,
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeInitialization(error) => {
                write!(formatter, "unable to initialize control runtime: {error}")
            }
            Self::Runtime(error) => Display::fmt(error, formatter),
            Self::ServiceNotFound(service) => {
                write!(formatter, "service `{service}` was not safely discovered")
            }
            Self::AmbiguousService(service) => write!(
                formatter,
                "service `{service}` exists in multiple scopes; select --runtime-scope system or --runtime-scope user"
            ),
            Self::ServiceLimit => formatter.write_str("runtime service discovery limit exceeded"),
            Self::Connect(error) => write!(formatter, "unable to connect to supervisor: {error}"),
            Self::ConnectTimeout => formatter.write_str("supervisor connect deadline exceeded"),
            Self::LifecycleTimeout => formatter.write_str("lifecycle completion deadline exceeded"),
            Self::LifecycleAbandoned(state) => write!(
                formatter,
                "supervisor settled in `{}`; the requested lifecycle goal is unreachable",
                state.name()
            ),
            Self::StatusUnavailable => {
                formatter.write_str("supervisor omitted the typed status payload")
            }
            Self::Transport(error) => Display::fmt(error, formatter),
            Self::Output(error) => write!(formatter, "unable to write output: {error}"),
            Self::Json(error) => write!(formatter, "unable to serialize JSON output: {error}"),
            Self::Remote(class) => {
                write!(
                    formatter,
                    "supervisor rejected the operation ({})",
                    class.value()
                )
            }
            Self::PartialFailure => formatter.write_str("one or more control operations failed"),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::RuntimeInitialization(error) | Self::Connect(error) | Self::Output(error) => {
                Some(error)
            }
            Self::Transport(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::ServiceNotFound(_)
            | Self::AmbiguousService(_)
            | Self::ServiceLimit
            | Self::ConnectTimeout
            | Self::LifecycleTimeout
            | Self::LifecycleAbandoned(_)
            | Self::StatusUnavailable
            | Self::Remote(_)
            | Self::PartialFailure => None,
        }
    }
}

impl From<RuntimeRootError> for ActionError {
    fn from(error: RuntimeRootError) -> Self {
        Self::Runtime(error)
    }
}

impl From<TransportError> for ActionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl ActionError {
    /// Stable process exit classification for this failure.
    #[must_use]
    pub fn exit_class(&self) -> ExitClass {
        match self {
            Self::RuntimeInitialization(_) | Self::Json(_) => ExitClass::Software,
            Self::Runtime(_) => ExitClass::Configuration,
            Self::ServiceNotFound(_) => ExitClass::NotFound,
            Self::LifecycleAbandoned(_) => ExitClass::Unavailable,
            Self::AmbiguousService(_) | Self::ServiceLimit => ExitClass::Data,
            Self::Connect(error) => match error.kind() {
                io::ErrorKind::PermissionDenied => ExitClass::Permission,
                io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset => ExitClass::Unavailable,
                _ => ExitClass::OsError,
            },
            Self::ConnectTimeout
            | Self::LifecycleTimeout
            | Self::Transport(TransportError::Timeout) => ExitClass::TemporaryFailure,
            Self::StatusUnavailable | Self::Transport(TransportError::Protocol(_)) => {
                ExitClass::Data
            }
            Self::Transport(TransportError::Io(_)) | Self::Output(_) => ExitClass::IoError,
            Self::Remote(class) => *class,
            Self::PartialFailure => ExitClass::PartialFailure,
        }
    }
}
