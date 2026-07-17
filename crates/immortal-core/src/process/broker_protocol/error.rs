//! Bounded protocol error returned by every broker request and event codec.
//!
//! [`BrokerProtocolError`] enumerates every way a frame can fail bounds or
//! structural validation: an oversized or truncated frame, an unsupported
//! version, an unknown message kind, a malformed field, or a value outside
//! its declared domain. No decoder path in this module ever panics or
//! indexes unchecked; every failure surfaces here instead.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use super::codec::{MAX_ARGUMENTS, MAX_ENVIRONMENT, MAX_SUPPLEMENTARY_GROUPS};
use super::framing::{MAX_FIELD_BYTES, MAX_FRAME_BYTES};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::process) enum BrokerProtocolError {
    FrameTooLarge(usize),
    Truncated,
    InvalidMagic,
    UnsupportedVersion(u8),
    UnknownKind(u8),
    LengthMismatch,
    TrailingBytes,
    FieldTooLarge(usize),
    TooManyArguments(usize),
    TooManyEnvironmentEntries(usize),
    InvalidGeneration,
    InvalidProcessId,
    InvalidProcessGroupId,
    InvalidStartupTimeout,
    InvalidReadinessTimeout,
    InvalidSignalTarget,
    InvalidSignal,
    InvalidSpawnStage,
    InvalidSpawnFailure,
    InvalidChildEvent,
    InvalidOptionalValue,
    InvalidBoolean,
    InvalidReadinessFailure,
    DuplicateEnvironmentKey,
    InvalidCredentials,
    TooManySupplementaryGroups(usize),
}

impl Display for BrokerProtocolError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::FrameTooLarge(actual) => {
                write!(
                    formatter,
                    "broker frame is {actual} bytes; limit is {MAX_FRAME_BYTES}"
                )
            }
            Self::Truncated => formatter.write_str("broker frame is truncated"),
            Self::InvalidMagic => formatter.write_str("invalid broker frame magic"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported broker protocol version {version}")
            }
            Self::UnknownKind(kind) => write!(formatter, "unknown broker message kind {kind}"),
            Self::LengthMismatch => {
                formatter.write_str("broker frame length does not match header")
            }
            Self::TrailingBytes => formatter.write_str("broker frame has trailing bytes"),
            Self::FieldTooLarge(actual) => {
                write!(
                    formatter,
                    "broker field is {actual} bytes; limit is {MAX_FIELD_BYTES}"
                )
            }
            Self::TooManyArguments(actual) => {
                write!(
                    formatter,
                    "broker command has {actual} arguments; limit is {MAX_ARGUMENTS}"
                )
            }
            Self::TooManyEnvironmentEntries(actual) => write!(
                formatter,
                "broker command has {actual} environment entries; limit is {MAX_ENVIRONMENT}"
            ),
            Self::InvalidGeneration => formatter.write_str("broker generation must be nonzero"),
            Self::InvalidProcessId => formatter.write_str("broker process ID must be positive"),
            Self::InvalidProcessGroupId => {
                formatter.write_str("broker process-group ID must be positive")
            }
            Self::InvalidStartupTimeout => {
                formatter.write_str("broker startup timeout is outside its supported range")
            }
            Self::InvalidReadinessTimeout => {
                formatter.write_str("broker readiness timeout is outside its supported range")
            }
            Self::InvalidSignalTarget => formatter.write_str("invalid broker signal target"),
            Self::InvalidSignal => formatter.write_str("invalid broker signal"),
            Self::InvalidSpawnStage => formatter.write_str("invalid broker spawn stage"),
            Self::InvalidSpawnFailure => formatter.write_str("invalid broker spawn failure"),
            Self::InvalidChildEvent => formatter.write_str("invalid broker child event"),
            Self::InvalidOptionalValue => formatter.write_str("invalid broker optional value flag"),
            Self::InvalidBoolean => formatter.write_str("invalid broker boolean value"),
            Self::InvalidReadinessFailure => {
                formatter.write_str("invalid broker readiness failure")
            }
            Self::DuplicateEnvironmentKey => {
                formatter.write_str("broker command contains a duplicate environment key")
            }
            Self::InvalidCredentials => {
                formatter.write_str("broker command contains invalid numeric credentials")
            }
            Self::TooManySupplementaryGroups(actual) => write!(
                formatter,
                "broker command has {actual} supplementary groups; limit is {MAX_SUPPLEMENTARY_GROUPS}"
            ),
        }
    }
}

impl Error for BrokerProtocolError {}
