//! Executor failure contract and conversions.
//!
//! This module keeps every fallible boundary classified without changing error
//! precedence. Process, daemon, broker, and state-machine sources remain
//! available through `source`, while supervisor-facing sentinel failures stay
//! stable and printable without exposing child modules.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
};

use super::{ChildEvent, DaemonError, ProcessBrokerError, ProcessBrokerEvent, TransitionError};

/// Failure to initialize or drive one foreground supervisor.
#[derive(Debug)]
pub enum ExecutorError {
    Unsupported(&'static str),
    OperatingSystem(io::Error),
    Daemon(DaemonError),
    Broker(ProcessBrokerError),
    Transition(TransitionError),
    BrokerTimedOut(&'static str),
    ContainmentFailed(ProcessBrokerEvent),
    UnexpectedBrokerEvent(ProcessBrokerEvent),
    BrokerExited(ChildEvent),
    ControlServerStopped,
}

impl Display for ExecutorError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(capability) => {
                write!(
                    formatter,
                    "foreground supervision does not yet support {capability}"
                )
            }
            Self::OperatingSystem(error) => Display::fmt(error, formatter),
            Self::Daemon(error) => Display::fmt(error, formatter),
            Self::Broker(error) => Display::fmt(error, formatter),
            Self::Transition(error) => Display::fmt(error, formatter),
            Self::BrokerTimedOut(operation) => {
                write!(formatter, "process broker timed out during {operation}")
            }
            Self::ContainmentFailed(event) => {
                write!(formatter, "process-group containment failed: {event:?}")
            }
            Self::UnexpectedBrokerEvent(event) => {
                write!(formatter, "unexpected process broker event: {event:?}")
            }
            Self::BrokerExited(event) => {
                write!(formatter, "process broker exited unsuccessfully: {event:?}")
            }
            Self::ControlServerStopped => {
                formatter.write_str("authenticated control server stopped unexpectedly")
            }
        }
    }
}

impl Error for ExecutorError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::OperatingSystem(error) => Some(error),
            Self::Daemon(error) => Some(error),
            Self::Broker(error) => Some(error),
            Self::Transition(error) => Some(error),
            Self::Unsupported(_)
            | Self::BrokerTimedOut(_)
            | Self::ContainmentFailed(_)
            | Self::UnexpectedBrokerEvent(_)
            | Self::BrokerExited(_)
            | Self::ControlServerStopped => None,
        }
    }
}

impl From<io::Error> for ExecutorError {
    fn from(error: io::Error) -> Self {
        Self::OperatingSystem(error)
    }
}

impl From<ProcessBrokerError> for ExecutorError {
    fn from(error: ProcessBrokerError) -> Self {
        Self::Broker(error)
    }
}

impl From<DaemonError> for ExecutorError {
    fn from(error: DaemonError) -> Self {
        Self::Daemon(error)
    }
}

impl From<TransitionError> for ExecutorError {
    fn from(error: TransitionError) -> Self {
        Self::Transition(error)
    }
}
