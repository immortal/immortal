//! Request and response models plus their bounded wire codecs.
//!
//! The control protocol accepts one authenticated request and returns one
//! bounded supervisor response. Encoding validates service-name safety,
//! generation guards, signal consistency, payload lengths, and typed status
//! fields before bytes cross the socket. Decoding walks the frame in protocol
//! order, rejects unknown versions and codes before later fields, and fails on
//! truncation or trailing bytes without changing supervisor state.

use crate::{
    service_name::is_safe_service_name,
    status::{
        LastResult, LoggerStatus, MAX_STATUS_ARGUMENTS, ReadinessStatus, ServiceState,
        StatusSnapshot, desired_state_code, desired_state_from_code,
    },
    supervisor::Generation,
};

use super::{
    GenerationMatch, MAX_FRAME_BYTES, MAX_SERVICE_NAME_BYTES, Operation, PROTOCOL_VERSION,
    ProtocolError, Signal, SignalScope,
    wire::{
        Cursor, HEADER_BYTES, MAGIC, RESPONSE_PAYLOAD_NONE, RESPONSE_PAYLOAD_STATUS,
        STATUS_BACKOFF, STATUS_DOWN_TIME, STATUS_KNOWN_FLAGS, STATUS_LAST_RESULT, STATUS_MAIN_PID,
        STATUS_SUPERVISOR_PID, STATUS_UPTIME,
    },
};

/// One authenticated request sent to a single supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Request {
    /// Requested operation.
    pub operation: Operation,
    /// Service name expected at the socket endpoint.
    pub service: String,
    /// Optimistic-concurrency generation condition.
    pub expected_generation: GenerationMatch,
    /// Signal target. Ignored for lifecycle operations.
    pub scope: SignalScope,
    /// Signal carried by [`Operation::Signal`].
    pub signal: Option<Signal>,
}

impl Request {
    /// Encode a request into a length-independent, bounded frame.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe service name, inconsistent signal fields,
    /// or a frame exceeding protocol bounds.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        validate_request(self)?;
        let name = self.service.as_bytes();
        let name_length = u16::try_from(name.len()).map_err(|_| ProtocolError::NameTooLong)?;
        let mut frame = Vec::with_capacity(HEADER_BYTES + name.len());
        frame.extend_from_slice(&MAGIC);
        frame.push(PROTOCOL_VERSION);
        frame.push(self.operation.code());
        frame.push(self.scope.code());
        frame.push(self.signal.map_or(0, Signal::code));
        frame.extend_from_slice(&self.expected_generation.encode().to_be_bytes());
        frame.extend_from_slice(&name_length.to_be_bytes());
        frame.extend_from_slice(name);
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(frame.len()));
        }
        Ok(frame)
    }

    /// Decode and validate one complete request frame.
    ///
    /// # Errors
    ///
    /// Returns an error for truncated, oversized, malformed, unsupported, or
    /// internally inconsistent input.
    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(frame.len()));
        }
        let mut cursor = Cursor::new(frame);
        if cursor.take::<4>()? != MAGIC {
            return Err(ProtocolError::InvalidMagic);
        }
        let version = cursor.byte()?;
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let operation_code = cursor.byte()?;
        let operation = Operation::from_code(operation_code)
            .ok_or(ProtocolError::UnknownOperation(operation_code))?;
        let scope_code = cursor.byte()?;
        let scope =
            SignalScope::from_code(scope_code).ok_or(ProtocolError::UnknownScope(scope_code))?;
        let signal_code = cursor.byte()?;
        let generation_value = u64::from_be_bytes(cursor.take::<8>()?);
        let name_length = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
        if name_length > MAX_SERVICE_NAME_BYTES {
            return Err(ProtocolError::NameTooLong);
        }
        let name_bytes = cursor.bytes(name_length)?;
        if !cursor.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        let service = std::str::from_utf8(name_bytes)
            .map_err(ProtocolError::InvalidUtf8)?
            .to_owned();
        let signal = if signal_code == 0 {
            None
        } else {
            Some(Signal::from_code(signal_code).ok_or(ProtocolError::UnknownSignal(signal_code))?)
        };
        let request = Self {
            operation,
            service,
            expected_generation: GenerationMatch::decode(generation_value),
            scope,
            signal,
        };
        validate_request(&request)?;
        Ok(request)
    }
}

pub(super) fn validate_request(request: &Request) -> Result<(), ProtocolError> {
    validate_service_name(&request.service)?;
    if request.operation != Operation::Status && request.expected_generation == GenerationMatch::Any
    {
        return Err(ProtocolError::MissingGenerationMatch);
    }
    match (request.operation, request.signal) {
        (Operation::Signal, Some(_)) | (Operation::Status, None) => Ok(()),
        (Operation::Signal, None) => Err(ProtocolError::MissingSignal),
        (_, Some(_)) => Err(ProtocolError::UnexpectedSignal),
        (_, None) => Ok(()),
    }
}

fn validate_service_name(name: &str) -> Result<(), ProtocolError> {
    if name.len() > MAX_SERVICE_NAME_BYTES {
        return Err(ProtocolError::NameTooLong);
    }
    if is_safe_service_name(name) {
        Ok(())
    } else {
        Err(ProtocolError::UnsafeServiceName)
    }
}

/// Stable result category returned by the privileged supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseCode {
    /// Operation completed successfully.
    Ok,
    /// Requested service or live child does not exist.
    NotFound,
    /// Peer credentials do not authorize this operation.
    PermissionDenied,
    /// Expected generation or desired state conflicts with current state.
    Conflict,
    /// Request is well-framed but invalid for the current state.
    Invalid,
    /// Supervisor encountered an internal operating-system failure.
    Internal,
}

impl ResponseCode {
    /// Stable lowercase name used by table and JSON clients.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not-found",
            Self::PermissionDenied => "permission-denied",
            Self::Conflict => "conflict",
            Self::Invalid => "invalid",
            Self::Internal => "internal",
        }
    }

    /// Whether the server completed the request successfully.
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Ok)
    }

    const fn code(self) -> u8 {
        match self {
            Self::Ok => 0,
            Self::NotFound => 1,
            Self::PermissionDenied => 2,
            Self::Conflict => 3,
            Self::Invalid => 4,
            Self::Internal => 5,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Ok),
            1 => Some(Self::NotFound),
            2 => Some(Self::PermissionDenied),
            3 => Some(Self::Conflict),
            4 => Some(Self::Invalid),
            5 => Some(Self::Internal),
            _ => None,
        }
    }
}

/// One bounded supervisor response.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Response {
    /// Stable result category.
    pub code: ResponseCode,
    /// Generation observed after processing the request.
    pub generation: Option<Generation>,
    /// Bounded UTF-8 diagnostic or status summary.
    pub message: String,
    /// Typed status payload, present only for successful status operations.
    pub status: Option<StatusSnapshot>,
}

impl Response {
    /// Encode one complete bounded response frame.
    ///
    /// # Errors
    ///
    /// Returns an error if the UTF-8 message cannot fit in the protocol frame.
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let message = self.message.as_bytes();
        let message_length =
            u16::try_from(message.len()).map_err(|_| ProtocolError::MessageTooLong)?;
        let mut frame = Vec::with_capacity(HEADER_BYTES + 2 + message.len());
        frame.extend_from_slice(&MAGIC);
        frame.push(PROTOCOL_VERSION);
        frame.push(self.code.code());
        frame.push(if self.status.is_some() {
            RESPONSE_PAYLOAD_STATUS
        } else {
            RESPONSE_PAYLOAD_NONE
        });
        frame.push(0);
        frame.extend_from_slice(&self.generation.map_or(0, Generation::get).to_be_bytes());
        frame.extend_from_slice(&[0, 0]);
        frame.extend_from_slice(&message_length.to_be_bytes());
        frame.extend_from_slice(message);
        if let Some(status) = &self.status {
            encode_status(status, &mut frame)?;
        }
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(frame.len()));
        }
        let payload_length = frame.len().saturating_sub(HEADER_BYTES);
        let encoded_payload_length =
            u16::try_from(payload_length).map_err(|_| ProtocolError::FrameTooLarge(frame.len()))?;
        frame
            .get_mut(HEADER_BYTES - 2..HEADER_BYTES)
            .ok_or(ProtocolError::Truncated)?
            .copy_from_slice(&encoded_payload_length.to_be_bytes());
        Ok(frame)
    }

    /// Decode one complete bounded response frame.
    ///
    /// # Errors
    ///
    /// Returns an error for malformed, truncated, unsupported, or oversized input.
    pub fn decode(frame: &[u8]) -> Result<Self, ProtocolError> {
        if frame.len() > MAX_FRAME_BYTES {
            return Err(ProtocolError::FrameTooLarge(frame.len()));
        }
        let mut cursor = Cursor::new(frame);
        if cursor.take::<4>()? != MAGIC {
            return Err(ProtocolError::InvalidMagic);
        }
        let version = cursor.byte()?;
        if version != PROTOCOL_VERSION {
            return Err(ProtocolError::UnsupportedVersion(version));
        }
        let code_value = cursor.byte()?;
        let code = ResponseCode::from_code(code_value)
            .ok_or(ProtocolError::UnknownResponseCode(code_value))?;
        let payload = cursor.byte()?;
        if cursor.byte()? != 0 {
            return Err(ProtocolError::InvalidReservedBits);
        }
        if !matches!(payload, RESPONSE_PAYLOAD_NONE | RESPONSE_PAYLOAD_STATUS) {
            return Err(ProtocolError::UnknownResponsePayload(payload));
        }
        let generation_value = u64::from_be_bytes(cursor.take::<8>()?);
        let payload_length = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
        if cursor.len() != payload_length {
            return Err(ProtocolError::TrailingBytes);
        }
        let message_length = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
        let message = std::str::from_utf8(cursor.bytes(message_length)?)
            .map_err(ProtocolError::InvalidUtf8)?
            .to_owned();
        let status = if payload == RESPONSE_PAYLOAD_STATUS {
            Some(decode_status(&mut cursor)?)
        } else {
            None
        };
        if !cursor.is_empty() {
            return Err(ProtocolError::TrailingBytes);
        }
        Ok(Self {
            code,
            generation: (generation_value != 0)
                .then_some(Generation::from_protocol(generation_value)),
            message,
            status,
        })
    }
}

fn encode_status(status: &StatusSnapshot, frame: &mut Vec<u8>) -> Result<(), ProtocolError> {
    if status.supervisor_pid == Some(0) || status.main_pid == Some(0) {
        return Err(ProtocolError::MalformedStatus("PID must be nonzero"));
    }
    let argument_count =
        u16::try_from(status.command.len()).map_err(|_| ProtocolError::TooManyStatusArguments)?;
    if status.command.len() > MAX_STATUS_ARGUMENTS {
        return Err(ProtocolError::TooManyStatusArguments);
    }
    let mut flags = 0_u16;
    flags |= status.supervisor_pid.map_or(0, |_| STATUS_SUPERVISOR_PID);
    flags |= status.main_pid.map_or(0, |_| STATUS_MAIN_PID);
    flags |= status.uptime_seconds.map_or(0, |_| STATUS_UPTIME);
    flags |= status.down_seconds.map_or(0, |_| STATUS_DOWN_TIME);
    flags |= status.backoff_seconds.map_or(0, |_| STATUS_BACKOFF);
    flags |= status.last_result.map_or(0, |_| STATUS_LAST_RESULT);
    frame.extend_from_slice(&flags.to_be_bytes());
    frame.push(desired_state_code(status.desired));
    frame.push(status.state.code());
    frame.push(status.readiness.code());
    frame.push(status.logger.code());
    if let Some(value) = status.supervisor_pid {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    if let Some(value) = status.main_pid {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    if let Some(value) = status.uptime_seconds {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    if let Some(value) = status.down_seconds {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    frame.extend_from_slice(&status.starts.to_be_bytes());
    frame.extend_from_slice(&status.failures.to_be_bytes());
    if let Some(value) = status.backoff_seconds {
        frame.extend_from_slice(&value.to_be_bytes());
    }
    if let Some(result) = status.last_result {
        let (kind, value) = result.code();
        frame.extend_from_slice(&[kind, value]);
    }
    frame.extend_from_slice(&argument_count.to_be_bytes());
    for argument in &status.command {
        let bytes = argument.as_bytes();
        let length =
            u16::try_from(bytes.len()).map_err(|_| ProtocolError::StatusArgumentTooLong)?;
        frame.extend_from_slice(&length.to_be_bytes());
        frame.extend_from_slice(bytes);
    }
    Ok(())
}

fn decode_status(cursor: &mut Cursor<'_>) -> Result<StatusSnapshot, ProtocolError> {
    let flags = u16::from_be_bytes(cursor.take::<2>()?);
    if flags & !STATUS_KNOWN_FLAGS != 0 {
        return Err(ProtocolError::MalformedStatus("unknown status flags"));
    }
    let desired = desired_state_from_code(cursor.byte()?)
        .ok_or(ProtocolError::MalformedStatus("unknown desired state"))?;
    let state = ServiceState::from_code(cursor.byte()?)
        .ok_or(ProtocolError::MalformedStatus("unknown service state"))?;
    let readiness = ReadinessStatus::from_code(cursor.byte()?)
        .ok_or(ProtocolError::MalformedStatus("unknown readiness state"))?;
    let logger = LoggerStatus::from_code(cursor.byte()?)
        .ok_or(ProtocolError::MalformedStatus("unknown logger state"))?;
    let supervisor_pid = decode_optional_u32(cursor, flags, STATUS_SUPERVISOR_PID)?;
    let main_pid = decode_optional_u32(cursor, flags, STATUS_MAIN_PID)?;
    let uptime_seconds = decode_optional_u64(cursor, flags, STATUS_UPTIME)?;
    let down_seconds = decode_optional_u64(cursor, flags, STATUS_DOWN_TIME)?;
    let starts = u64::from_be_bytes(cursor.take::<8>()?);
    let failures = u64::from_be_bytes(cursor.take::<8>()?);
    let backoff_seconds = decode_optional_u64(cursor, flags, STATUS_BACKOFF)?;
    let last_result = if flags & STATUS_LAST_RESULT != 0 {
        LastResult::from_code(cursor.byte()?, cursor.byte()?)
            .ok_or(ProtocolError::MalformedStatus("unknown last-result code"))
            .map(Some)?
    } else {
        None
    };
    let argument_count = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
    if argument_count > MAX_STATUS_ARGUMENTS {
        return Err(ProtocolError::TooManyStatusArguments);
    }
    let mut command = Vec::with_capacity(argument_count);
    for _ in 0..argument_count {
        let length = usize::from(u16::from_be_bytes(cursor.take::<2>()?));
        command.push(
            std::str::from_utf8(cursor.bytes(length)?)
                .map_err(ProtocolError::InvalidUtf8)?
                .to_owned(),
        );
    }
    Ok(StatusSnapshot {
        supervisor_pid,
        main_pid,
        desired,
        state,
        readiness,
        uptime_seconds,
        down_seconds,
        starts,
        failures,
        last_result,
        backoff_seconds,
        logger,
        command,
    })
}

fn decode_optional_u32(
    cursor: &mut Cursor<'_>,
    flags: u16,
    flag: u16,
) -> Result<Option<u32>, ProtocolError> {
    if flags & flag == 0 {
        return Ok(None);
    }
    let value = u32::from_be_bytes(cursor.take::<4>()?);
    if value == 0 {
        return Err(ProtocolError::MalformedStatus("PID must be nonzero"));
    }
    Ok(Some(value))
}

fn decode_optional_u64(
    cursor: &mut Cursor<'_>,
    flags: u16,
    flag: u16,
) -> Result<Option<u64>, ProtocolError> {
    (flags & flag != 0)
        .then(|| cursor.take::<8>().map(u64::from_be_bytes))
        .transpose()
}
