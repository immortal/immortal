//! Typed reconciliation failures.
//!
//! `ServiceFailure` is the isolated, per-service failure retained after a
//! complete scan so unrelated work continues; `ServiceFailureKind` classifies
//! its cause; `MutationError` separates an isolated service failure from loss
//! of the runtime root, which must fail the reconciliation loop closed.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
};

use immortal_core::{
    control::{ResponseCode, TransportError},
    reconcile::{DependencyError, LauncherError},
    runtime::RuntimeRootError,
    status::{LoggerLastResult, SpawnDetail, StatusSnapshot},
};

/// One isolated service mutation failure retained after a complete scan.
#[derive(Debug)]
pub struct ServiceFailure {
    service: String,
    kind: ServiceFailureKind,
}

impl ServiceFailure {
    pub(super) fn new(service: &str, kind: ServiceFailureKind) -> Self {
        Self {
            service: service.to_owned(),
            kind,
        }
    }
}

impl Display for ServiceFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "service `{}`: {}", self.service, self.kind)
    }
}

impl Error for ServiceFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.kind.source()
    }
}

/// Classified cause of a single isolated service mutation failure.
#[derive(Debug)]
pub(super) enum ServiceFailureKind {
    Io(io::Error),
    Dependency(DependencyError),
    Launcher(LauncherError),
    Transport(TransportError),
    Remote(ResponseCode),
    /// The service did not reach its goal state in time; the blocker is what
    /// its last status named as the reason, when it named one.
    LifecycleTimeout(Option<StartBlocker>),
    ActiveWithoutControl,
}

/// What a supervisor's status names as keeping its service from starting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartBlocker {
    /// The service process could not be created.
    Service(SpawnDetail),
    /// A logger stage failed, and a service waits for its loggers.
    Logger(LoggerLastResult),
}

impl StartBlocker {
    /// Read the blocker from one status snapshot; a logger failure is named
    /// first because the service is not attempted while its loggers are down.
    pub(super) fn from_status(status: &StatusSnapshot) -> Option<Self> {
        status
            .logger_last_result
            .map(Self::Logger)
            .or_else(|| status.last_spawn_failure.map(Self::Service))
    }
}

impl Display for StartBlocker {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(detail) => write!(formatter, "service {}", detail.describe()),
            Self::Logger(result) => write!(formatter, "logger {}", result.describe()),
        }
    }
}

impl Display for ServiceFailureKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, formatter),
            Self::Dependency(error) => Display::fmt(error, formatter),
            Self::Launcher(error) => Display::fmt(error, formatter),
            Self::Transport(error) => Display::fmt(error, formatter),
            Self::Remote(code) => write!(formatter, "mutation rejected: {}", code.name()),
            Self::LifecycleTimeout(None) => formatter.write_str("lifecycle deadline exceeded"),
            Self::LifecycleTimeout(Some(blocker)) => {
                write!(formatter, "lifecycle deadline exceeded: {blocker}")
            }
            Self::ActiveWithoutControl => {
                formatter.write_str("supervisor lock is active but control socket is unavailable")
            }
        }
    }
}

impl Error for ServiceFailureKind {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Dependency(error) => Some(error),
            Self::Launcher(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Remote(_) | Self::LifecycleTimeout(_) | Self::ActiveWithoutControl => None,
        }
    }
}

impl From<DependencyError> for ServiceFailureKind {
    fn from(error: DependencyError) -> Self {
        Self::Dependency(error)
    }
}

impl From<io::Error> for ServiceFailureKind {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<TransportError> for ServiceFailureKind {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

/// Separates an isolated service failure from loss of the runtime root.
pub(super) enum MutationError {
    Isolated(ServiceFailureKind),
    RuntimeLost(RuntimeRootError),
}

impl From<ServiceFailureKind> for MutationError {
    fn from(error: ServiceFailureKind) -> Self {
        Self::Isolated(error)
    }
}

impl From<io::Error> for MutationError {
    fn from(error: io::Error) -> Self {
        Self::Isolated(error.into())
    }
}
