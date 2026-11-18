//! Exhaustive failure contract returned by every broker client operation.
//!
//! [`ProcessBrokerError`] wraps I/O failures, wire-protocol violations, an
//! unexpectedly closed `SIGCHLD` stream, and a reaped process this broker
//! never spawned. [`ProcessBrokerError::is_end_of_stream`] distinguishes an
//! ordinary supervisor disconnect (which triggers cleanup) from every other
//! I/O failure (which is fatal to the broker). A disconnect is observable from
//! either direction: a read sees `UnexpectedEof` or `ConnectionReset`, while a
//! write to the departed peer sees `BrokenPipe`. All three must reach cleanup,
//! because a descriptor-tracked generation's configured stop command runs only
//! on that path.
//!
//! Only a failure on the supervisor socket itself can mean the supervisor is
//! gone, so the wire layer reports socket I/O as its own connection kind. The
//! same error kinds raised anywhere else, such as a group helper's socket while
//! a guard is released, stay ordinary I/O failures: misreading one of them as a
//! departed supervisor would kill every service of a supervisor that is still
//! connected.

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io;

use super::{BrokerProtocolError, ProcessId};

/// Failure to create or communicate with the dedicated process broker.
#[derive(Debug)]
pub struct ProcessBrokerError(pub(super) ProcessBrokerErrorKind);

#[derive(Debug)]
pub(super) enum ProcessBrokerErrorKind {
    /// Read or write failure on the supervisor socket.
    Connection(io::Error),
    Io(io::Error),
    Protocol(BrokerProtocolError),
    SignalStreamClosed,
    UnownedChild(ProcessId),
}

impl ProcessBrokerError {
    /// Whether this failure means the supervisor connection is simply gone.
    ///
    /// Covers both directions of the supervisor socket: a read observing
    /// `UnexpectedEof` or `ConnectionReset`, and a write observing
    /// `BrokenPipe`. The same kinds from any other descriptor are not a
    /// disconnect.
    pub(super) fn is_end_of_stream(&self) -> bool {
        matches!(
            &self.0,
            ProcessBrokerErrorKind::Connection(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe
                        | io::ErrorKind::ConnectionReset
                        | io::ErrorKind::UnexpectedEof
                )
        )
    }

    /// Construct the synthetic disconnect used when the reader task ends
    /// without having queued an error of its own.
    pub(super) fn disconnected() -> Self {
        Self::connection(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "supervisor control connection closed",
        ))
    }

    /// Classify a failed read or write on the supervisor socket.
    pub(super) const fn connection(error: io::Error) -> Self {
        Self(ProcessBrokerErrorKind::Connection(error))
    }
}

impl Display for ProcessBrokerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ProcessBrokerErrorKind::Connection(error) | ProcessBrokerErrorKind::Io(error) => {
                write!(formatter, "broker I/O failed: {error}")
            }
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
            ProcessBrokerErrorKind::Connection(error) | ProcessBrokerErrorKind::Io(error) => {
                Some(error)
            }
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
