//! Private, bounded wire contract between one supervisor and its process broker.

use std::{
    collections::BTreeMap,
    error::Error,
    ffi::{OsStr, OsString},
    fmt::{self, Display, Formatter},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::PathBuf,
    time::Duration,
};

use crate::supervisor::Generation;

use super::{
    BrokerLoggerId, ChildEvent, ProcessCommand, ProcessCredentials, ProcessGroupId, ProcessId,
    ProcessSignal, SpawnFailure, SpawnStage, SupplementaryGroups,
};

const MAGIC: [u8; 4] = *b"IMBR";
pub(super) const HEADER_BYTES: usize = 10;
const VERSION: u8 = 4;
const REQUEST_SPAWN: u8 = 1;
const REQUEST_SIGNAL: u8 = 2;
const REQUEST_SHUTDOWN: u8 = 3;
const REQUEST_DETACH: u8 = 4;
const REQUEST_SPAWN_LOGGER: u8 = 5;
const REQUEST_CLOSE_LOGGER_INPUTS: u8 = 6;
const EVENT_STARTED: u8 = 1;
const EVENT_SPAWN_FAILED: u8 = 2;
const EVENT_CHILD: u8 = 3;
const EVENT_SIGNAL_DELIVERED: u8 = 4;
const EVENT_SIGNAL_FAILED: u8 = 5;
const EVENT_SHUTDOWN_COMPLETE: u8 = 6;
const EVENT_SHUTDOWN_FAILED: u8 = 7;
const EVENT_READY: u8 = 8;
const EVENT_DETACHED: u8 = 9;
const EVENT_DETACH_FAILED: u8 = 10;
const EVENT_GENERATION_READY: u8 = 11;
const EVENT_READINESS_FAILED: u8 = 12;
const EVENT_LOGGER_INPUTS_CLOSED: u8 = 13;
const CHILD_EXITED: u8 = 1;
const CHILD_SIGNALED: u8 = 2;
const CHILD_STOPPED: u8 = 3;
const CHILD_CONTINUED: u8 = 4;
const MAX_ARGUMENTS: usize = 4_096;
const MAX_ENVIRONMENT: usize = 4_096;
const MAX_SUPPLEMENTARY_GROUPS: usize = 65_536;
const MAX_FIELD_BYTES: usize = 256 * 1024;
const MAX_STARTUP_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_READINESS_TIMEOUT: Duration = Duration::from_hours(24);

pub(super) const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// Logical target resolved against the broker's currently owned generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerSignalTarget {
    Process,
    Group,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerReadinessFailure {
    Timeout,
    Descriptor,
    InvalidToken,
}

/// Request sent from the Tokio supervisor to its single-threaded broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum BrokerRequest {
    Spawn {
        generation: Generation,
        command: ProcessCommand,
        startup_timeout: Duration,
        readiness_timeout: Option<Duration>,
    },
    Signal {
        generation: Generation,
        target: BrokerSignalTarget,
        signal: ProcessSignal,
    },
    Detach {
        generation: Generation,
    },
    SpawnLogger {
        generation: Generation,
        logger: BrokerLoggerId,
        startup_timeout: Duration,
    },
    CloseLoggerInputs,
    Shutdown,
}

impl BrokerRequest {
    pub(super) fn encode(&self) -> Result<Vec<u8>, BrokerProtocolError> {
        let mut payload = Vec::new();
        let kind = match self {
            Self::Spawn {
                generation,
                command,
                startup_timeout,
                readiness_timeout,
            } => {
                encode_generation(*generation, &mut payload);
                encode_timeout(*startup_timeout, &mut payload)?;
                encode_optional_timeout(*readiness_timeout, &mut payload)?;
                encode_command(command, &mut payload)?;
                REQUEST_SPAWN
            }
            Self::Signal {
                generation,
                target,
                signal,
            } => {
                encode_generation(*generation, &mut payload);
                payload.push(match target {
                    BrokerSignalTarget::Process => 1,
                    BrokerSignalTarget::Group => 2,
                });
                payload.push(signal.code());
                REQUEST_SIGNAL
            }
            Self::Detach { generation } => {
                encode_generation(*generation, &mut payload);
                REQUEST_DETACH
            }
            Self::SpawnLogger {
                generation,
                logger,
                startup_timeout,
            } => {
                encode_generation(*generation, &mut payload);
                payload.extend_from_slice(&logger.pipeline().to_be_bytes());
                payload.extend_from_slice(&logger.stage().to_be_bytes());
                encode_timeout(*startup_timeout, &mut payload)?;
                REQUEST_SPAWN_LOGGER
            }
            Self::CloseLoggerInputs => REQUEST_CLOSE_LOGGER_INPUTS,
            Self::Shutdown => REQUEST_SHUTDOWN,
        };
        encode_frame(kind, &payload)
    }

    pub(super) fn decode(frame: &[u8]) -> Result<Self, BrokerProtocolError> {
        let (kind, payload) = decode_frame(frame)?;
        let mut cursor = Cursor::new(payload);
        let request = match kind {
            REQUEST_SPAWN => Self::Spawn {
                generation: decode_generation(&mut cursor)?,
                startup_timeout: decode_timeout(&mut cursor)?,
                readiness_timeout: decode_optional_timeout(&mut cursor)?,
                command: decode_command(&mut cursor)?,
            },
            REQUEST_SIGNAL => {
                let generation = decode_generation(&mut cursor)?;
                let target = match cursor.byte()? {
                    1 => BrokerSignalTarget::Process,
                    2 => BrokerSignalTarget::Group,
                    _ => return Err(BrokerProtocolError::InvalidSignalTarget),
                };
                let signal = ProcessSignal::from_code(cursor.byte()?)
                    .ok_or(BrokerProtocolError::InvalidSignal)?;
                Self::Signal {
                    generation,
                    target,
                    signal,
                }
            }
            REQUEST_SHUTDOWN => Self::Shutdown,
            REQUEST_DETACH => Self::Detach {
                generation: decode_generation(&mut cursor)?,
            },
            REQUEST_SPAWN_LOGGER => Self::SpawnLogger {
                generation: decode_generation(&mut cursor)?,
                logger: BrokerLoggerId::new(
                    u16::from_be_bytes(cursor.take::<2>()?),
                    u16::from_be_bytes(cursor.take::<2>()?),
                ),
                startup_timeout: decode_timeout(&mut cursor)?,
            },
            REQUEST_CLOSE_LOGGER_INPUTS => Self::CloseLoggerInputs,
            other => return Err(BrokerProtocolError::UnknownKind(other)),
        };
        cursor.finish()?;
        Ok(request)
    }
}

/// Event sent from the process broker to the Tokio supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum BrokerEvent {
    Ready,
    Started {
        generation: Generation,
        process: ProcessId,
        group: ProcessGroupId,
    },
    SpawnFailed {
        generation: Generation,
        stage: SpawnStage,
        failure: SpawnFailure,
        os_error: Option<i32>,
        cleanup_pending: Option<ProcessId>,
    },
    Child {
        generation: Generation,
        event: ChildEvent,
    },
    SignalDelivered {
        generation: Generation,
    },
    SignalFailed {
        generation: Generation,
        os_error: Option<i32>,
    },
    Detached {
        generation: Generation,
    },
    DetachFailed {
        generation: Generation,
    },
    GenerationReady {
        generation: Generation,
    },
    ReadinessFailed {
        generation: Generation,
        failure: BrokerReadinessFailure,
    },
    LoggerInputsClosed,
    ShutdownComplete,
    ShutdownFailed {
        os_error: Option<i32>,
    },
}

impl BrokerEvent {
    pub(super) fn encode(&self) -> Result<Vec<u8>, BrokerProtocolError> {
        let mut payload = Vec::new();
        let kind = match self {
            Self::Ready => EVENT_READY,
            Self::Started {
                generation,
                process,
                group,
            } => {
                encode_generation(*generation, &mut payload);
                encode_process(*process, &mut payload);
                encode_group(*group, &mut payload);
                EVENT_STARTED
            }
            Self::SpawnFailed {
                generation,
                stage,
                failure,
                os_error,
                cleanup_pending,
            } => {
                encode_generation(*generation, &mut payload);
                payload.push(spawn_stage_code(*stage));
                payload.push(spawn_failure_code(*failure));
                encode_optional_error(*os_error, &mut payload);
                encode_optional_process(*cleanup_pending, &mut payload);
                EVENT_SPAWN_FAILED
            }
            Self::Child { generation, event } => {
                encode_generation(*generation, &mut payload);
                encode_child_event(*event, &mut payload);
                EVENT_CHILD
            }
            Self::SignalDelivered { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_SIGNAL_DELIVERED
            }
            Self::SignalFailed {
                generation,
                os_error,
            } => {
                encode_generation(*generation, &mut payload);
                encode_optional_error(*os_error, &mut payload);
                EVENT_SIGNAL_FAILED
            }
            Self::Detached { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_DETACHED
            }
            Self::DetachFailed { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_DETACH_FAILED
            }
            Self::GenerationReady { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_GENERATION_READY
            }
            Self::ReadinessFailed {
                generation,
                failure,
            } => {
                encode_generation(*generation, &mut payload);
                payload.push(match failure {
                    BrokerReadinessFailure::Timeout => 1,
                    BrokerReadinessFailure::Descriptor => 2,
                    BrokerReadinessFailure::InvalidToken => 3,
                });
                EVENT_READINESS_FAILED
            }
            Self::LoggerInputsClosed => EVENT_LOGGER_INPUTS_CLOSED,
            Self::ShutdownComplete => EVENT_SHUTDOWN_COMPLETE,
            Self::ShutdownFailed { os_error } => {
                encode_optional_error(*os_error, &mut payload);
                EVENT_SHUTDOWN_FAILED
            }
        };
        encode_frame(kind, &payload)
    }

    pub(super) fn decode(frame: &[u8]) -> Result<Self, BrokerProtocolError> {
        let (kind, payload) = decode_frame(frame)?;
        let mut cursor = Cursor::new(payload);
        let event = match kind {
            EVENT_READY => Self::Ready,
            EVENT_STARTED => Self::Started {
                generation: decode_generation(&mut cursor)?,
                process: decode_process(&mut cursor)?,
                group: decode_group(&mut cursor)?,
            },
            EVENT_SPAWN_FAILED => Self::SpawnFailed {
                generation: decode_generation(&mut cursor)?,
                stage: spawn_stage_from_code(cursor.byte()?)?,
                failure: spawn_failure_from_code(cursor.byte()?)?,
                os_error: decode_optional_error(&mut cursor)?,
                cleanup_pending: decode_optional_process(&mut cursor)?,
            },
            EVENT_CHILD => Self::Child {
                generation: decode_generation(&mut cursor)?,
                event: decode_child_event(&mut cursor)?,
            },
            EVENT_SIGNAL_DELIVERED => Self::SignalDelivered {
                generation: decode_generation(&mut cursor)?,
            },
            EVENT_SIGNAL_FAILED => Self::SignalFailed {
                generation: decode_generation(&mut cursor)?,
                os_error: decode_optional_error(&mut cursor)?,
            },
            EVENT_DETACHED => Self::Detached {
                generation: decode_generation(&mut cursor)?,
            },
            EVENT_DETACH_FAILED => Self::DetachFailed {
                generation: decode_generation(&mut cursor)?,
            },
            EVENT_GENERATION_READY => Self::GenerationReady {
                generation: decode_generation(&mut cursor)?,
            },
            EVENT_READINESS_FAILED => Self::ReadinessFailed {
                generation: decode_generation(&mut cursor)?,
                failure: match cursor.byte()? {
                    1 => BrokerReadinessFailure::Timeout,
                    2 => BrokerReadinessFailure::Descriptor,
                    3 => BrokerReadinessFailure::InvalidToken,
                    _ => return Err(BrokerProtocolError::InvalidReadinessFailure),
                },
            },
            EVENT_LOGGER_INPUTS_CLOSED => Self::LoggerInputsClosed,
            EVENT_SHUTDOWN_COMPLETE => Self::ShutdownComplete,
            EVENT_SHUTDOWN_FAILED => Self::ShutdownFailed {
                os_error: decode_optional_error(&mut cursor)?,
            },
            other => return Err(BrokerProtocolError::UnknownKind(other)),
        };
        cursor.finish()?;
        Ok(event)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum BrokerProtocolError {
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

fn encode_frame(kind: u8, payload: &[u8]) -> Result<Vec<u8>, BrokerProtocolError> {
    let frame_length = HEADER_BYTES.saturating_add(payload.len());
    if frame_length > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame_length));
    }
    let payload_length = u32::try_from(payload.len())
        .map_err(|_| BrokerProtocolError::FrameTooLarge(frame_length))?;
    let mut frame = Vec::with_capacity(frame_length);
    frame.extend_from_slice(&MAGIC);
    frame.push(VERSION);
    frame.push(kind);
    frame.extend_from_slice(&payload_length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

fn decode_frame(frame: &[u8]) -> Result<(u8, &[u8]), BrokerProtocolError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame.len()));
    }
    let mut cursor = Cursor::new(frame);
    if cursor.take::<4>()? != MAGIC {
        return Err(BrokerProtocolError::InvalidMagic);
    }
    let version = cursor.byte()?;
    if version != VERSION {
        return Err(BrokerProtocolError::UnsupportedVersion(version));
    }
    let kind = cursor.byte()?;
    let payload_length = usize::try_from(u32::from_be_bytes(cursor.take::<4>()?))
        .map_err(|_| BrokerProtocolError::LengthMismatch)?;
    if cursor.remaining() != payload_length {
        return Err(BrokerProtocolError::LengthMismatch);
    }
    Ok((kind, cursor.bytes(payload_length)?))
}

pub(super) fn declared_frame_length(
    header: &[u8; HEADER_BYTES],
) -> Result<usize, BrokerProtocolError> {
    if header.get(..4) != Some(MAGIC.as_slice()) {
        return Err(BrokerProtocolError::InvalidMagic);
    }
    let version = header
        .get(4)
        .copied()
        .ok_or(BrokerProtocolError::Truncated)?;
    if version != VERSION {
        return Err(BrokerProtocolError::UnsupportedVersion(version));
    }
    let length_bytes: [u8; 4] = header
        .get(6..10)
        .ok_or(BrokerProtocolError::Truncated)?
        .try_into()
        .map_err(|_| BrokerProtocolError::Truncated)?;
    let payload_length = usize::try_from(u32::from_be_bytes(length_bytes))
        .map_err(|_| BrokerProtocolError::LengthMismatch)?;
    let frame_length = HEADER_BYTES.saturating_add(payload_length);
    if frame_length > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame_length));
    }
    Ok(frame_length)
}

fn encode_command(
    command: &ProcessCommand,
    payload: &mut Vec<u8>,
) -> Result<(), BrokerProtocolError> {
    encode_os(command.program(), payload)?;
    if command.arguments().len() > MAX_ARGUMENTS {
        return Err(BrokerProtocolError::TooManyArguments(
            command.arguments().len(),
        ));
    }
    let argument_count = u16::try_from(command.arguments().len())
        .map_err(|_| BrokerProtocolError::TooManyArguments(command.arguments().len()))?;
    payload.extend_from_slice(&argument_count.to_be_bytes());
    for argument in command.arguments() {
        encode_os(argument, payload)?;
    }
    if command.resolved_environment().len() > MAX_ENVIRONMENT {
        return Err(BrokerProtocolError::TooManyEnvironmentEntries(
            command.resolved_environment().len(),
        ));
    }
    let environment_count = u16::try_from(command.resolved_environment().len()).map_err(|_| {
        BrokerProtocolError::TooManyEnvironmentEntries(command.resolved_environment().len())
    })?;
    payload.extend_from_slice(&environment_count.to_be_bytes());
    for (key, value) in command.resolved_environment() {
        encode_os(key, payload)?;
        encode_os(value, payload)?;
    }
    match command.requested_working_directory() {
        Some(directory) => {
            payload.push(1);
            encode_os(directory.as_os_str(), payload)?;
        }
        None => payload.push(0),
    }
    encode_credentials(command.requested_credentials(), payload)?;
    Ok(())
}

fn decode_command(cursor: &mut Cursor<'_>) -> Result<ProcessCommand, BrokerProtocolError> {
    let program = cursor.os_string()?;
    let argument_count = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
    if argument_count > MAX_ARGUMENTS {
        return Err(BrokerProtocolError::TooManyArguments(argument_count));
    }
    let mut arguments = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        arguments.push(cursor.os_string()?);
    }
    let environment_count = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
    if environment_count > MAX_ENVIRONMENT {
        return Err(BrokerProtocolError::TooManyEnvironmentEntries(
            environment_count,
        ));
    }
    let mut environment = BTreeMap::new();
    for _ in 0..environment_count {
        let key = cursor.os_string()?;
        let value = cursor.os_string()?;
        if environment.insert(key, value).is_some() {
            return Err(BrokerProtocolError::DuplicateEnvironmentKey);
        }
    }
    let working_directory = match cursor.byte()? {
        0 => None,
        1 => Some(PathBuf::from(cursor.os_string()?)),
        _ => return Err(BrokerProtocolError::InvalidOptionalValue),
    };
    let credentials = decode_credentials(cursor)?;
    Ok(ProcessCommand {
        program,
        arguments,
        environment,
        working_directory,
        credentials,
    })
}

fn encode_credentials(
    credentials: Option<&ProcessCredentials>,
    payload: &mut Vec<u8>,
) -> Result<(), BrokerProtocolError> {
    let Some(credentials) = credentials else {
        payload.push(0);
        return Ok(());
    };
    payload.push(1);
    payload.extend_from_slice(&credentials.user().to_be_bytes());
    payload.extend_from_slice(&credentials.group().to_be_bytes());
    match credentials.supplementary_groups() {
        SupplementaryGroups::Preserve => payload.push(0),
        SupplementaryGroups::Set(groups) => {
            payload.push(1);
            if groups.len() > MAX_SUPPLEMENTARY_GROUPS {
                return Err(BrokerProtocolError::TooManySupplementaryGroups(
                    groups.len(),
                ));
            }
            let count = u32::try_from(groups.len())
                .map_err(|_| BrokerProtocolError::TooManySupplementaryGroups(groups.len()))?;
            payload.extend_from_slice(&count.to_be_bytes());
            for group in groups {
                payload.extend_from_slice(&group.to_be_bytes());
            }
        }
    }
    Ok(())
}

fn decode_credentials(
    cursor: &mut Cursor<'_>,
) -> Result<Option<ProcessCredentials>, BrokerProtocolError> {
    match cursor.byte()? {
        0 => Ok(None),
        1 => {
            let user = libc::uid_t::from_be_bytes(cursor.take::<4>()?);
            let group = libc::gid_t::from_be_bytes(cursor.take::<4>()?);
            let supplementary_groups = match cursor.byte()? {
                0 => SupplementaryGroups::Preserve,
                1 => {
                    let count = usize::try_from(u32::from_be_bytes(cursor.take::<4>()?))
                        .map_err(|_| BrokerProtocolError::InvalidCredentials)?;
                    if count > MAX_SUPPLEMENTARY_GROUPS {
                        return Err(BrokerProtocolError::TooManySupplementaryGroups(count));
                    }
                    let mut groups = Vec::with_capacity(count);
                    for _ in 0..count {
                        groups.push(libc::gid_t::from_be_bytes(cursor.take::<4>()?));
                    }
                    SupplementaryGroups::Set(groups)
                }
                _ => return Err(BrokerProtocolError::InvalidCredentials),
            };
            Ok(Some(ProcessCredentials::new(
                user,
                group,
                supplementary_groups,
            )))
        }
        _ => Err(BrokerProtocolError::InvalidCredentials),
    }
}

fn encode_os(value: &OsStr, payload: &mut Vec<u8>) -> Result<(), BrokerProtocolError> {
    let bytes = value.as_bytes();
    if bytes.len() > MAX_FIELD_BYTES {
        return Err(BrokerProtocolError::FieldTooLarge(bytes.len()));
    }
    let length =
        u32::try_from(bytes.len()).map_err(|_| BrokerProtocolError::FieldTooLarge(bytes.len()))?;
    payload.extend_from_slice(&length.to_be_bytes());
    payload.extend_from_slice(bytes);
    Ok(())
}

fn encode_timeout(timeout: Duration, payload: &mut Vec<u8>) -> Result<(), BrokerProtocolError> {
    if timeout.is_zero() || timeout > MAX_STARTUP_TIMEOUT {
        return Err(BrokerProtocolError::InvalidStartupTimeout);
    }
    let milliseconds = u64::try_from(timeout.as_millis())
        .map_err(|_| BrokerProtocolError::InvalidStartupTimeout)?;
    if milliseconds == 0 {
        return Err(BrokerProtocolError::InvalidStartupTimeout);
    }
    payload.extend_from_slice(&milliseconds.to_be_bytes());
    Ok(())
}

fn decode_timeout(cursor: &mut Cursor<'_>) -> Result<Duration, BrokerProtocolError> {
    let timeout = Duration::from_millis(u64::from_be_bytes(cursor.take::<8>()?));
    if timeout.is_zero() || timeout > MAX_STARTUP_TIMEOUT {
        return Err(BrokerProtocolError::InvalidStartupTimeout);
    }
    Ok(timeout)
}

fn encode_optional_timeout(
    timeout: Option<Duration>,
    payload: &mut Vec<u8>,
) -> Result<(), BrokerProtocolError> {
    let milliseconds = match timeout {
        Some(timeout) if !timeout.is_zero() && timeout <= MAX_READINESS_TIMEOUT => {
            u64::try_from(timeout.as_millis())
                .map_err(|_| BrokerProtocolError::InvalidReadinessTimeout)?
        }
        Some(_) => return Err(BrokerProtocolError::InvalidReadinessTimeout),
        None => 0,
    };
    payload.extend_from_slice(&milliseconds.to_be_bytes());
    Ok(())
}

fn decode_optional_timeout(
    cursor: &mut Cursor<'_>,
) -> Result<Option<Duration>, BrokerProtocolError> {
    let milliseconds = u64::from_be_bytes(cursor.take::<8>()?);
    if milliseconds == 0 {
        return Ok(None);
    }
    let timeout = Duration::from_millis(milliseconds);
    if timeout > MAX_READINESS_TIMEOUT {
        return Err(BrokerProtocolError::InvalidReadinessTimeout);
    }
    Ok(Some(timeout))
}

fn encode_generation(generation: Generation, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&generation.get().to_be_bytes());
}

fn decode_generation(cursor: &mut Cursor<'_>) -> Result<Generation, BrokerProtocolError> {
    Generation::new(u64::from_be_bytes(cursor.take::<8>()?))
        .ok_or(BrokerProtocolError::InvalidGeneration)
}

fn encode_process(process: ProcessId, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&process.get().to_be_bytes());
}

fn decode_process(cursor: &mut Cursor<'_>) -> Result<ProcessId, BrokerProtocolError> {
    ProcessId::new(i32::from_be_bytes(cursor.take::<4>()?))
        .ok_or(BrokerProtocolError::InvalidProcessId)
}

fn encode_group(group: ProcessGroupId, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&group.get().to_be_bytes());
}

fn decode_group(cursor: &mut Cursor<'_>) -> Result<ProcessGroupId, BrokerProtocolError> {
    ProcessGroupId::new(i32::from_be_bytes(cursor.take::<4>()?))
        .ok_or(BrokerProtocolError::InvalidProcessGroupId)
}

fn encode_optional_process(process: Option<ProcessId>, payload: &mut Vec<u8>) {
    match process {
        Some(process) => {
            payload.push(1);
            encode_process(process, payload);
        }
        None => payload.push(0),
    }
}

fn decode_optional_process(
    cursor: &mut Cursor<'_>,
) -> Result<Option<ProcessId>, BrokerProtocolError> {
    match cursor.byte()? {
        0 => Ok(None),
        1 => decode_process(cursor).map(Some),
        _ => Err(BrokerProtocolError::InvalidOptionalValue),
    }
}

fn encode_optional_error(error: Option<i32>, payload: &mut Vec<u8>) {
    match error {
        Some(error) => {
            payload.push(1);
            payload.extend_from_slice(&error.to_be_bytes());
        }
        None => payload.push(0),
    }
}

fn decode_optional_error(cursor: &mut Cursor<'_>) -> Result<Option<i32>, BrokerProtocolError> {
    match cursor.byte()? {
        0 => Ok(None),
        1 => Ok(Some(i32::from_be_bytes(cursor.take::<4>()?))),
        _ => Err(BrokerProtocolError::InvalidOptionalValue),
    }
}

fn encode_child_event(event: ChildEvent, payload: &mut Vec<u8>) {
    let (kind, process, detail) = match event {
        ChildEvent::Exited { pid, code } => (CHILD_EXITED, pid, code),
        ChildEvent::Signaled { pid, signal } => (CHILD_SIGNALED, pid, signal),
        ChildEvent::Stopped { pid, signal } => (CHILD_STOPPED, pid, signal),
        ChildEvent::Continued { pid } => (CHILD_CONTINUED, pid, 0),
    };
    payload.push(kind);
    encode_process(process, payload);
    payload.push(detail);
}

fn decode_child_event(cursor: &mut Cursor<'_>) -> Result<ChildEvent, BrokerProtocolError> {
    let kind = cursor.byte()?;
    let pid = decode_process(cursor)?;
    let detail = cursor.byte()?;
    match kind {
        CHILD_EXITED => Ok(ChildEvent::Exited { pid, code: detail }),
        CHILD_SIGNALED if detail > 0 => Ok(ChildEvent::Signaled {
            pid,
            signal: detail,
        }),
        CHILD_STOPPED if detail > 0 => Ok(ChildEvent::Stopped {
            pid,
            signal: detail,
        }),
        CHILD_CONTINUED if detail == 0 => Ok(ChildEvent::Continued { pid }),
        _ => Err(BrokerProtocolError::InvalidChildEvent),
    }
}

const fn spawn_stage_code(stage: SpawnStage) -> u8 {
    match stage {
        SpawnStage::Specification => 1,
        SpawnStage::Fork => 2,
        SpawnStage::ProcessGroup => 3,
        SpawnStage::CurrentDirectory => 4,
        SpawnStage::DescriptorMapping => 5,
        SpawnStage::Execute => 6,
        SpawnStage::StartupHandshake => 7,
        SpawnStage::Identity => 8,
        SpawnStage::SignalState => 9,
    }
}

fn spawn_stage_from_code(code: u8) -> Result<SpawnStage, BrokerProtocolError> {
    match code {
        1 => Ok(SpawnStage::Specification),
        2 => Ok(SpawnStage::Fork),
        3 => Ok(SpawnStage::ProcessGroup),
        4 => Ok(SpawnStage::CurrentDirectory),
        5 => Ok(SpawnStage::DescriptorMapping),
        6 => Ok(SpawnStage::Execute),
        7 => Ok(SpawnStage::StartupHandshake),
        8 => Ok(SpawnStage::Identity),
        9 => Ok(SpawnStage::SignalState),
        _ => Err(BrokerProtocolError::InvalidSpawnStage),
    }
}

const fn spawn_failure_code(failure: SpawnFailure) -> u8 {
    match failure {
        SpawnFailure::OperatingSystem => 1,
        SpawnFailure::TimedOut => 2,
        SpawnFailure::InvalidHandshake => 3,
        SpawnFailure::InvalidForkContract => 4,
    }
}

fn spawn_failure_from_code(code: u8) -> Result<SpawnFailure, BrokerProtocolError> {
    match code {
        1 => Ok(SpawnFailure::OperatingSystem),
        2 => Ok(SpawnFailure::TimedOut),
        3 => Ok(SpawnFailure::InvalidHandshake),
        4 => Ok(SpawnFailure::InvalidForkContract),
        _ => Err(BrokerProtocolError::InvalidSpawnFailure),
    }
}

struct Cursor<'frame> {
    frame: &'frame [u8],
    position: usize,
}

impl<'frame> Cursor<'frame> {
    const fn new(frame: &'frame [u8]) -> Self {
        Self { frame, position: 0 }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], BrokerProtocolError> {
        let bytes = self.bytes(N)?;
        bytes.try_into().map_err(|_| BrokerProtocolError::Truncated)
    }

    fn byte(&mut self) -> Result<u8, BrokerProtocolError> {
        Ok(self.take::<1>()?[0])
    }

    fn bytes(&mut self, length: usize) -> Result<&'frame [u8], BrokerProtocolError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(BrokerProtocolError::Truncated)?;
        let bytes = self
            .frame
            .get(self.position..end)
            .ok_or(BrokerProtocolError::Truncated)?;
        self.position = end;
        Ok(bytes)
    }

    fn os_string(&mut self) -> Result<OsString, BrokerProtocolError> {
        let length = usize::try_from(u32::from_be_bytes(self.take::<4>()?))
            .map_err(|_| BrokerProtocolError::FieldTooLarge(usize::MAX))?;
        if length > MAX_FIELD_BYTES {
            return Err(BrokerProtocolError::FieldTooLarge(length));
        }
        Ok(OsString::from_vec(self.bytes(length)?.to_vec()))
    }

    const fn remaining(&self) -> usize {
        self.frame.len().saturating_sub(self.position)
    }

    fn finish(&self) -> Result<(), BrokerProtocolError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(BrokerProtocolError::TrailingBytes)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error, ffi::OsString, io, os::unix::ffi::OsStringExt, path::PathBuf, time::Duration,
    };

    use crate::supervisor::Generation;

    use super::{
        BrokerEvent, BrokerProtocolError, BrokerReadinessFailure, BrokerRequest,
        BrokerSignalTarget, MAX_FRAME_BYTES,
    };
    use crate::process::{
        BrokerLoggerId, ChildEvent, ProcessCommand, ProcessCredentials, ProcessEnvironment,
        ProcessGroupId, ProcessId, ProcessSignal, SpawnFailure, SpawnStage, SupplementaryGroups,
    };

    #[test]
    fn spawn_request_round_trips_non_utf8_operating_system_values() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(7).ok_or("invalid test generation")?;
        let mut command = ProcessCommand::new(OsString::from_vec(vec![b'/', b'x', 0xff]));
        command
            .argument(OsString::from_vec(vec![b'a', 0xfe]))
            .working_directory(PathBuf::from(OsString::from_vec(vec![b'/', b'd', 0xfd])))
            .credentials(ProcessCredentials::new(
                123,
                456,
                SupplementaryGroups::Set(vec![456, 789]),
            ));
        let mut environment = ProcessEnvironment::new();
        environment.insert(
            OsString::from_vec(vec![b'K', 0xfc]),
            OsString::from_vec(vec![b'V', 0xfb]),
        );
        command.environment(environment);
        let request = BrokerRequest::Spawn {
            generation,
            command,
            startup_timeout: Duration::from_millis(1_500),
            readiness_timeout: Some(Duration::from_secs(2)),
        };
        assert_eq!(BrokerRequest::decode(&request.encode()?)?, request);
        Ok(())
    }

    #[test]
    fn logger_signal_detach_and_shutdown_requests_round_trip() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(9).ok_or("invalid test generation")?;
        for request in [
            BrokerRequest::Signal {
                generation,
                target: BrokerSignalTarget::Process,
                signal: ProcessSignal::User1,
            },
            BrokerRequest::Signal {
                generation,
                target: BrokerSignalTarget::Group,
                signal: ProcessSignal::Terminate,
            },
            BrokerRequest::Detach { generation },
            BrokerRequest::SpawnLogger {
                generation,
                logger: BrokerLoggerId::new(1, 2),
                startup_timeout: Duration::from_secs(2),
            },
            BrokerRequest::CloseLoggerInputs,
            BrokerRequest::Shutdown,
        ] {
            assert_eq!(BrokerRequest::decode(&request.encode()?)?, request);
        }
        Ok(())
    }

    #[test]
    fn every_broker_event_round_trips() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(11).ok_or("invalid test generation")?;
        let process = ProcessId::new(123).ok_or("invalid test process")?;
        let group = ProcessGroupId::new(123).ok_or("invalid test process group")?;
        for event in [
            BrokerEvent::Ready,
            BrokerEvent::Started {
                generation,
                process,
                group,
            },
            BrokerEvent::SpawnFailed {
                generation,
                stage: SpawnStage::Execute,
                failure: SpawnFailure::OperatingSystem,
                os_error: Some(2),
                cleanup_pending: None,
            },
            BrokerEvent::Child {
                generation,
                event: ChildEvent::Exited {
                    pid: process,
                    code: 42,
                },
            },
            BrokerEvent::Child {
                generation,
                event: ChildEvent::Signaled {
                    pid: process,
                    signal: 15,
                },
            },
            BrokerEvent::Child {
                generation,
                event: ChildEvent::Stopped {
                    pid: process,
                    signal: 19,
                },
            },
            BrokerEvent::Child {
                generation,
                event: ChildEvent::Continued { pid: process },
            },
            BrokerEvent::SignalDelivered { generation },
            BrokerEvent::SignalFailed {
                generation,
                os_error: Some(3),
            },
            BrokerEvent::Detached { generation },
            BrokerEvent::DetachFailed { generation },
            BrokerEvent::GenerationReady { generation },
            BrokerEvent::ReadinessFailed {
                generation,
                failure: BrokerReadinessFailure::Timeout,
            },
            BrokerEvent::LoggerInputsClosed,
            BrokerEvent::ShutdownComplete,
            BrokerEvent::ShutdownFailed { os_error: None },
        ] {
            assert_eq!(BrokerEvent::decode(&event.encode()?)?, event);
        }
        Ok(())
    }

    #[test]
    fn malformed_and_oversized_frames_fail_closed() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(1).ok_or("invalid test generation")?;
        let request = BrokerRequest::Signal {
            generation,
            target: BrokerSignalTarget::Process,
            signal: ProcessSignal::Terminate,
        };
        let frame = request.encode()?;
        for length in 0..frame.len() {
            let truncated = frame
                .get(..length)
                .ok_or_else(|| io::Error::other("invalid truncation length"))?;
            assert!(BrokerRequest::decode(truncated).is_err());
        }
        let mut trailing = frame.clone();
        trailing.push(0);
        assert_eq!(
            BrokerRequest::decode(&trailing),
            Err(BrokerProtocolError::LengthMismatch)
        );
        assert_eq!(
            BrokerRequest::decode(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(BrokerProtocolError::FrameTooLarge(MAX_FRAME_BYTES + 1))
        );
        Ok(())
    }

    #[test]
    fn invalid_generation_signal_and_timeout_are_rejected() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(1).ok_or("invalid test generation")?;
        let signal = BrokerRequest::Signal {
            generation,
            target: BrokerSignalTarget::Process,
            signal: ProcessSignal::Terminate,
        };
        let mut frame = signal.encode()?;
        frame
            .get_mut(10..18)
            .ok_or_else(|| io::Error::other("missing generation bytes"))?
            .fill(0);
        assert_eq!(
            BrokerRequest::decode(&frame),
            Err(BrokerProtocolError::InvalidGeneration)
        );

        let mut frame = signal.encode()?;
        *frame
            .get_mut(19)
            .ok_or_else(|| io::Error::other("missing signal byte"))? = 0;
        assert_eq!(
            BrokerRequest::decode(&frame),
            Err(BrokerProtocolError::InvalidSignal)
        );

        let spawn = BrokerRequest::Spawn {
            generation,
            command: ProcessCommand::new("/bin/true"),
            startup_timeout: Duration::from_secs(1),
            readiness_timeout: None,
        };
        let mut frame = spawn.encode()?;
        frame
            .get_mut(18..26)
            .ok_or_else(|| io::Error::other("missing startup-timeout bytes"))?
            .fill(0);
        assert_eq!(
            BrokerRequest::decode(&frame),
            Err(BrokerProtocolError::InvalidStartupTimeout)
        );

        let spawn = BrokerRequest::Spawn {
            generation,
            command: ProcessCommand::new("/bin/true"),
            startup_timeout: Duration::from_secs(1),
            readiness_timeout: Some(Duration::from_secs(1)),
        };
        let mut frame = spawn.encode()?;
        frame
            .get_mut(26..34)
            .ok_or_else(|| io::Error::other("missing readiness-timeout bytes"))?
            .fill(u8::MAX);
        assert_eq!(
            BrokerRequest::decode(&frame),
            Err(BrokerProtocolError::InvalidReadinessTimeout)
        );
        Ok(())
    }
}
