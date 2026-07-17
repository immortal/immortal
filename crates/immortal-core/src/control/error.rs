//! Typed failures for control protocol decoding and validation.
//!
//! Every request and response codec reports malformed frames, unsupported
//! versions, unknown codes, inconsistent request fields, invalid UTF-8, and
//! bounded status violations through this enum. Decoders fail closed before
//! handing data to lifecycle decision logic, preserving protocol bounds and
//! validation precedence across the trust boundary.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    str::Utf8Error,
};

use super::MAX_FRAME_BYTES;

/// Protocol decoding or validation error.
#[derive(Debug)]
pub enum ProtocolError {
    /// Frame exceeds `MAX_FRAME_BYTES`.
    FrameTooLarge(usize),
    /// Frame ended before a declared field was complete.
    Truncated,
    /// Magic bytes do not identify the Immortal protocol.
    InvalidMagic,
    /// Peer uses a protocol version this implementation does not support.
    UnsupportedVersion(u8),
    /// Operation code is not defined by this protocol version.
    UnknownOperation(u8),
    /// Signal scope code is invalid.
    UnknownScope(u8),
    /// Signal code is invalid.
    UnknownSignal(u8),
    /// Response result code is invalid.
    UnknownResponseCode(u8),
    /// Response payload tag is not defined by this protocol version.
    UnknownResponsePayload(u8),
    /// Signal operation omitted its signal.
    MissingSignal,
    /// A mutating operation omitted its optimistic generation condition.
    MissingGenerationMatch,
    /// A non-signal operation carried a signal.
    UnexpectedSignal,
    /// Service name exceeds `MAX_SERVICE_NAME_BYTES`.
    NameTooLong,
    /// Response message cannot fit in a frame.
    MessageTooLong,
    /// Status contains more command arguments than the bounded decoder accepts.
    TooManyStatusArguments,
    /// One status command argument cannot fit its length field.
    StatusArgumentTooLong,
    /// Typed status fields violate the protocol contract.
    MalformedStatus(&'static str),
    /// Service name is empty or unsafe for runtime-directory lookup.
    UnsafeServiceName,
    /// A text field is not valid UTF-8.
    InvalidUtf8(Utf8Error),
    /// Complete declared frame was followed by unparsed data.
    TrailingBytes,
    /// Reserved protocol header bits were nonzero.
    InvalidReservedBits,
}

impl Display for ProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge(actual) => write!(
                formatter,
                "control frame is {actual} bytes; limit is {MAX_FRAME_BYTES} bytes"
            ),
            Self::Truncated => formatter.write_str("truncated control frame"),
            Self::InvalidMagic => formatter.write_str("invalid control protocol magic"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported control protocol version {version}")
            }
            Self::UnknownOperation(code) => write!(formatter, "unknown control operation {code}"),
            Self::UnknownScope(code) => write!(formatter, "unknown signal scope {code}"),
            Self::UnknownSignal(code) => write!(formatter, "unknown signal {code}"),
            Self::UnknownResponseCode(code) => write!(formatter, "unknown response code {code}"),
            Self::UnknownResponsePayload(code) => {
                write!(formatter, "unknown response payload {code}")
            }
            Self::MissingSignal => formatter.write_str("signal operation omitted its signal"),
            Self::MissingGenerationMatch => {
                formatter.write_str("mutating control operation omitted its generation condition")
            }
            Self::UnexpectedSignal => {
                formatter.write_str("non-signal operation unexpectedly carried a signal")
            }
            Self::NameTooLong => formatter.write_str("service name is too long"),
            Self::MessageTooLong => formatter.write_str("response message is too long"),
            Self::TooManyStatusArguments => {
                formatter.write_str("status contains too many command arguments")
            }
            Self::StatusArgumentTooLong => {
                formatter.write_str("status command argument is too long")
            }
            Self::MalformedStatus(reason) => write!(formatter, "malformed status: {reason}"),
            Self::UnsafeServiceName => formatter.write_str("service name is unsafe"),
            Self::InvalidUtf8(error) => write!(formatter, "control text is not UTF-8: {error}"),
            Self::TrailingBytes => formatter.write_str("control frame has trailing bytes"),
            Self::InvalidReservedBits => {
                formatter.write_str("control frame has nonzero reserved bits")
            }
        }
    }
}

impl Error for ProtocolError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUtf8(error) => Some(error),
            Self::FrameTooLarge(_)
            | Self::Truncated
            | Self::InvalidMagic
            | Self::UnsupportedVersion(_)
            | Self::UnknownOperation(_)
            | Self::UnknownScope(_)
            | Self::UnknownSignal(_)
            | Self::UnknownResponseCode(_)
            | Self::UnknownResponsePayload(_)
            | Self::MissingSignal
            | Self::MissingGenerationMatch
            | Self::UnexpectedSignal
            | Self::NameTooLong
            | Self::MessageTooLong
            | Self::TooManyStatusArguments
            | Self::StatusArgumentTooLong
            | Self::MalformedStatus(_)
            | Self::UnsafeServiceName
            | Self::TrailingBytes
            | Self::InvalidReservedBits => None,
        }
    }
}
