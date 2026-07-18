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
    reconcile::LauncherError,
    runtime::RuntimeRootError,
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
    Launcher(LauncherError),
    Transport(TransportError),
    Remote(ResponseCode),
    LifecycleTimeout,
    ActiveWithoutControl,
}

impl Display for ServiceFailureKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, formatter),
            Self::Launcher(error) => Display::fmt(error, formatter),
            Self::Transport(error) => Display::fmt(error, formatter),
            Self::Remote(code) => write!(formatter, "mutation rejected: {}", code.name()),
            Self::LifecycleTimeout => formatter.write_str("lifecycle deadline exceeded"),
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
            Self::Launcher(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Remote(_) | Self::LifecycleTimeout | Self::ActiveWithoutControl => None,
        }
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
