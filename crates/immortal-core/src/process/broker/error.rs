//! Exhaustive failure contract returned by every broker client operation.
//!
//! [`ProcessBrokerError`] wraps I/O failures, wire-protocol violations, an
//! unexpectedly closed `SIGCHLD` stream, and a reaped process this broker
//! never spawned. [`ProcessBrokerError::is_end_of_stream`] distinguishes an
//! ordinary supervisor disconnect (which triggers cleanup) from every other
//! I/O failure (which is fatal to the broker).

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;

use super::{BrokerProtocolError, ProcessId};

/// Failure to create or communicate with the dedicated process broker.
#[derive(Debug)]
pub struct ProcessBrokerError(pub(super) ProcessBrokerErrorKind);

#[derive(Debug)]
pub(super) enum ProcessBrokerErrorKind {
    Io(io::Error),
    Protocol(BrokerProtocolError),
    SignalStreamClosed,
    UnownedChild(ProcessId),
}

impl ProcessBrokerError {
    pub(super) fn is_end_of_stream(&self) -> bool {
        matches!(
            &self.0,
            ProcessBrokerErrorKind::Io(error)
                if error.kind() == io::ErrorKind::UnexpectedEof
                    || error.kind() == io::ErrorKind::ConnectionReset
        )
    }
}

impl Display for ProcessBrokerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ProcessBrokerErrorKind::Io(error) => write!(formatter, "broker I/O failed: {error}"),
            ProcessBrokerErrorKind::Protocol(error) => {
                write!(formatter, "broker protocol failed: {error}")
            }
            ProcessBrokerErrorKind::SignalStreamClosed => {
                formatter.write_str("broker SIGCHLD stream closed")
            }
            ProcessBrokerErrorKind::UnownedChild(process) => {
                write!(formatter, "broker reaped unowned child {process}")
            }
        }
    }
}

impl Error for ProcessBrokerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.0 {
            ProcessBrokerErrorKind::Io(error) => Some(error),
            ProcessBrokerErrorKind::Protocol(error) => Some(error),
            ProcessBrokerErrorKind::SignalStreamClosed
            | ProcessBrokerErrorKind::UnownedChild(_) => None,
        }
    }
}

impl From<io::Error> for ProcessBrokerError {
    fn from(error: io::Error) -> Self {
        Self(ProcessBrokerErrorKind::Io(error))
    }
}

impl From<BrokerProtocolError> for ProcessBrokerError {
    fn from(error: BrokerProtocolError) -> Self {
        Self(ProcessBrokerErrorKind::Protocol(error))
    }
}
