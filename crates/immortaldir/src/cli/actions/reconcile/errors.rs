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
    status::{LoggerLastResult, ServiceState, SpawnDetail, StatusSnapshot},
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
    /// its status showed as the current reason while a start was awaited.
    LifecycleTimeout(Option<StartBlocker>),
    ActiveWithoutControl,
}

/// What currently keeps a service from becoming ready, as its status shows.
///
/// Status retains the last terminal result as history after a later
/// generation starts, so a retained failure is a blocker only while the
/// lifecycle state says nothing has replaced it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StartBlocker {
    /// The service process could not be created.
    Service(SpawnDetail),
    /// A logger stage failed, and a service waits for its loggers.
    Logger(LoggerLastResult),
    /// The service executed and has not declared readiness.
    Readiness,
}

impl StartBlocker {
    /// Fold one poll of a service into the blocker known so far.
    ///
    /// `status` is `None` when the supervisor or its status is absent, which
    /// forgets any earlier cause: nothing observed still supports it.
    ///
    /// - Without a service process, a failed logger is named first because
    ///   the service is not attempted while its loggers are down, then the
    ///   failed start, which status publishes only while it is the most
    ///   recent terminal result.
    /// - While a new generation is being created the outcome is not known
    ///   yet, so the `previous` blocker stands.
    /// - Once the service executed, an earlier failed start is history and a
    ///   logger failure no longer holds it back; only readiness is pending.
    pub(super) fn observe(previous: Option<Self>, status: Option<&StatusSnapshot>) -> Option<Self> {
        let status = status?;
        match status.state {
            ServiceState::Initializing
            | ServiceState::Down
            | ServiceState::WaitingCondition
            | ServiceState::Backoff
            | ServiceState::Failed => status
                .logger_last_result
                .map(Self::Logger)
                .or_else(|| status.last_spawn_failure.map(Self::Service)),
            ServiceState::Starting => previous,
            ServiceState::Running => Some(Self::Readiness),
            ServiceState::Ready
            | ServiceState::Paused
            | ServiceState::Stopping
            | ServiceState::Completed
            | ServiceState::Exited => None,
        }
    }
}

impl Display for StartBlocker {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Service(detail) => write!(formatter, "service {}", detail.describe()),
            Self::Logger(result) => write!(formatter, "logger {}", result.describe()),
            Self::Readiness => formatter.write_str("service has not declared readiness"),
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
