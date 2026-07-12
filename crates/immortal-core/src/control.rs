//! Bounded, versioned local control protocol.
//!
//! The privileged supervisor accepts a small binary request vocabulary. JSON
//! formatting belongs to `immortalctl`, after the server response has crossed
//! the authenticated Unix-socket boundary.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    str::Utf8Error,
    time::Duration,
};

#[cfg(unix)]
use std::{
    fs,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};
#[cfg(unix)]
use tokio::{
    net::{UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
};

use crate::{
    status::{
        LastResult, LoggerStatus, MAX_STATUS_ARGUMENTS, ReadinessStatus, ServiceState,
        StatusSnapshot, desired_state_code, desired_state_from_code,
    },
    supervisor::{DesiredState, Generation, StateMachine, SupervisorState},
};

const MAGIC: [u8; 4] = *b"IMMO";
const HEADER_BYTES: usize = 18;
const RESPONSE_PAYLOAD_NONE: u8 = 0;
const RESPONSE_PAYLOAD_STATUS: u8 = 1;
const STATUS_SUPERVISOR_PID: u16 = 1 << 0;
const STATUS_MAIN_PID: u16 = 1 << 1;
const STATUS_UPTIME: u16 = 1 << 2;
const STATUS_DOWN_TIME: u16 = 1 << 3;
const STATUS_BACKOFF: u16 = 1 << 4;
const STATUS_LAST_RESULT: u16 = 1 << 5;
const STATUS_KNOWN_FLAGS: u16 = STATUS_SUPERVISOR_PID
    | STATUS_MAIN_PID
    | STATUS_UPTIME
    | STATUS_DOWN_TIME
    | STATUS_BACKOFF
    | STATUS_LAST_RESULT;

/// Current control-protocol version.
pub const PROTOCOL_VERSION: u8 = 1;
/// Hard upper bound for any request or response frame.
pub const MAX_FRAME_BYTES: usize = 64 * 1024;
/// Hard upper bound for a UTF-8 service name.
pub const MAX_SERVICE_NAME_BYTES: usize = 255;
/// Maximum idle time for one control-frame read or write.
pub const CONTROL_IO_TIMEOUT: Duration = Duration::from_secs(5);
/// Default number of concurrently handled control connections.
pub const DEFAULT_MAX_CONTROL_CLIENTS: usize = 32;

/// Control operation accepted by a supervisor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Operation {
    /// Inspect status without mutation.
    Status,
    /// Set persistent desired state to Up.
    Start,
    /// Stop the process group and remain supervised Down.
    Stop,
    /// Stop, reap, and create a new generation.
    Restart,
    /// Run one generation and remain Down afterward.
    Once,
    /// Leave the service running and exit its supervisor.
    Exit,
    /// Stop the process group and exit its supervisor.
    Halt,
    /// Deliver the signal carried in [`Request::signal`].
    Signal,
}

impl Operation {
    const fn code(self) -> u8 {
        match self {
            Self::Status => 1,
            Self::Start => 2,
            Self::Stop => 3,
            Self::Restart => 4,
            Self::Once => 5,
            Self::Exit => 6,
            Self::Halt => 7,
            Self::Signal => 8,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Status),
            2 => Some(Self::Start),
            3 => Some(Self::Stop),
            4 => Some(Self::Restart),
            5 => Some(Self::Once),
            6 => Some(Self::Exit),
            7 => Some(Self::Halt),
            8 => Some(Self::Signal),
            _ => None,
        }
    }
}

/// Explicit target for raw signal delivery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SignalScope {
    /// Signal only the main child.
    #[default]
    Main,
    /// Signal the entire owned process group.
    Group,
}

impl SignalScope {
    const fn code(self) -> u8 {
        match self {
            Self::Main => 1,
            Self::Group => 2,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Main),
            2 => Some(Self::Group),
            _ => None,
        }
    }
}

/// Portable signal vocabulary exposed by `immortalctl`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Signal {
    /// `SIGUSR1` (`-1`).
    User1,
    /// `SIGUSR2` (`-2`).
    User2,
    /// `SIGALRM` (`-a`).
    Alarm,
    /// `SIGCONT` (`-c`).
    Continue,
    /// `SIGHUP` (`-h`).
    Hangup,
    /// `SIGINT` (`-i`).
    Interrupt,
    /// `SIGKILL` (`-k`).
    Kill,
    /// `SIGTTIN` (`-in`).
    TerminalInput,
    /// `SIGTTOU` (`-ou`).
    TerminalOutput,
    /// `SIGQUIT` (`-q`).
    Quit,
    /// `SIGSTOP` (`-s`).
    Stop,
    /// `SIGTERM` (`-t`).
    Terminate,
    /// `SIGWINCH` (`-w`).
    WindowChange,
}

/// Optimistic-concurrency condition attached to a request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum GenerationMatch {
    /// Do not compare child generation. Reserved for read-only status requests.
    #[default]
    Any,
    /// Require that the supervisor currently has no child generation.
    NoChild,
    /// Require one exact live or transitioning generation.
    Exact(Generation),
}

impl GenerationMatch {
    const NO_CHILD_SENTINEL: u64 = u64::MAX;

    const fn encode(self) -> u64 {
        match self {
            Self::Any => 0,
            Self::NoChild => Self::NO_CHILD_SENTINEL,
            Self::Exact(generation) => generation.get(),
        }
    }

    const fn decode(value: u64) -> Self {
        match value {
            0 => Self::Any,
            Self::NO_CHILD_SENTINEL => Self::NoChild,
            generation => Self::Exact(Generation::from_protocol(generation)),
        }
    }

    /// Whether this condition matches the supervisor's current generation.
    #[must_use]
    pub const fn matches(self, current: Option<Generation>) -> bool {
        match (self, current) {
            (Self::Any, _) | (Self::NoChild, None) => true,
            (Self::Exact(expected), Some(actual)) => expected.get() == actual.get(),
            (Self::NoChild, Some(_)) | (Self::Exact(_), None) => false,
        }
    }
}

impl Signal {
    /// Parse the stable case-insensitive command name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        if name.eq_ignore_ascii_case("usr1") {
            Some(Self::User1)
        } else if name.eq_ignore_ascii_case("usr2") {
            Some(Self::User2)
        } else if name.eq_ignore_ascii_case("alrm") {
            Some(Self::Alarm)
        } else if name.eq_ignore_ascii_case("cont") {
            Some(Self::Continue)
        } else if name.eq_ignore_ascii_case("hup") {
            Some(Self::Hangup)
        } else if name.eq_ignore_ascii_case("int") {
            Some(Self::Interrupt)
        } else if name.eq_ignore_ascii_case("kill") {
            Some(Self::Kill)
        } else if name.eq_ignore_ascii_case("ttin") {
            Some(Self::TerminalInput)
        } else if name.eq_ignore_ascii_case("ttou") {
            Some(Self::TerminalOutput)
        } else if name.eq_ignore_ascii_case("quit") {
            Some(Self::Quit)
        } else if name.eq_ignore_ascii_case("stop") {
            Some(Self::Stop)
        } else if name.eq_ignore_ascii_case("term") {
            Some(Self::Terminate)
        } else if name.eq_ignore_ascii_case("winch") {
            Some(Self::WindowChange)
        } else {
            None
        }
    }

    /// Stable lowercase name used in diagnostics and structured output.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::User1 => "usr1",
            Self::User2 => "usr2",
            Self::Alarm => "alrm",
            Self::Continue => "cont",
            Self::Hangup => "hup",
            Self::Interrupt => "int",
            Self::Kill => "kill",
            Self::TerminalInput => "ttin",
            Self::TerminalOutput => "ttou",
            Self::Quit => "quit",
            Self::Stop => "stop",
            Self::Terminate => "term",
            Self::WindowChange => "winch",
        }
    }

    const fn code(self) -> u8 {
        match self {
            Self::User1 => 1,
            Self::User2 => 2,
            Self::Alarm => 3,
            Self::Continue => 4,
            Self::Hangup => 5,
            Self::Interrupt => 6,
            Self::Kill => 7,
            Self::TerminalInput => 8,
            Self::TerminalOutput => 9,
            Self::Quit => 10,
            Self::Stop => 11,
            Self::Terminate => 12,
            Self::WindowChange => 13,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::User1),
            2 => Some(Self::User2),
            3 => Some(Self::Alarm),
            4 => Some(Self::Continue),
            5 => Some(Self::Hangup),
            6 => Some(Self::Interrupt),
            7 => Some(Self::Kill),
            8 => Some(Self::TerminalInput),
            9 => Some(Self::TerminalOutput),
            10 => Some(Self::Quit),
            11 => Some(Self::Stop),
            12 => Some(Self::Terminate),
            13 => Some(Self::WindowChange),
            _ => None,
        }
    }
}

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

/// Action the process executor must complete after accepting a request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlEffect {
    /// Status or an idempotent lifecycle request needs no executor work.
    None,
    /// Begin dependency/condition evaluation and then create a generation.
    BeginStart,
    /// Cancel a pending condition or backoff, optionally starting immediately.
    CancelPending {
        /// Start again after cancellation.
        start: bool,
    },
    /// Stop and reap the owned process group, then apply the completion action.
    StopGroup {
        /// Exact generation to stop.
        generation: Generation,
        /// Desired action after reaping.
        after: StopCompletion,
    },
    /// Exit the supervisor, optionally leaving a live child deliberately orphaned.
    ExitSupervisor {
        /// Whether the current child/process group must remain running.
        leave_child: bool,
    },
    /// Deliver a raw signal without changing desired state.
    DeliverSignal {
        /// Exact live generation checked by the request.
        generation: Generation,
        /// Main process or owned group target.
        scope: SignalScope,
        /// Signal to deliver.
        signal: Signal,
    },
}

/// Lifecycle action after a stopped group has been fully reaped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopCompletion {
    /// Remain supervised Down.
    Down,
    /// Begin a replacement generation.
    Restart,
    /// Exit the supervisor after cleanup.
    Halt,
}

/// Validated response plus executor work selected for a request.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlDecision {
    /// Immediate acceptance or rejection response.
    pub response: Response,
    /// Work which must complete for an accepted mutation.
    pub effect: ControlEffect,
}

/// Validate a request against one supervisor and update operator intent.
///
/// This function never performs process or socket I/O. The caller must execute
/// [`ControlDecision::effect`] and publish completion before treating an
/// accepted lifecycle operation as complete.
pub fn decide_request(
    service_name: &str,
    machine: &mut StateMachine,
    request: &Request,
) -> ControlDecision {
    if request.service != service_name {
        return rejected(machine, ResponseCode::NotFound, "service name mismatch");
    }
    if let Err(error) = validate_request(request) {
        return rejected(machine, ResponseCode::Invalid, &error.to_string());
    }
    let generation = machine.state().generation();
    if !request.expected_generation.matches(generation) {
        return rejected(
            machine,
            ResponseCode::Conflict,
            "service generation changed",
        );
    }

    let effect = match request.operation {
        Operation::Status => return status_decision(machine, generation),
        Operation::Start => {
            if matches!(machine.state(), SupervisorState::Failed(_))
                && machine.reset_failure().is_err()
            {
                return rejected(machine, ResponseCode::Internal, "unable to reset failure");
            }
            machine.set_desired(DesiredState::Up);
            start_effect(machine.state())
        }
        Operation::Once => {
            if matches!(machine.state(), SupervisorState::Failed(_))
                && machine.reset_failure().is_err()
            {
                return rejected(machine, ResponseCode::Internal, "unable to reset failure");
            }
            machine.set_desired(DesiredState::Once);
            start_effect(machine.state())
        }
        Operation::Stop => {
            machine.set_desired(DesiredState::Down);
            stop_effect(machine.state(), StopCompletion::Down)
        }
        Operation::Restart => {
            if matches!(machine.state(), SupervisorState::Failed(_))
                && machine.reset_failure().is_err()
            {
                return rejected(machine, ResponseCode::Internal, "unable to reset failure");
            }
            machine.set_desired(DesiredState::Up);
            machine.state().live_generation().map_or_else(
                || start_effect(machine.state()),
                |generation| ControlEffect::StopGroup {
                    generation,
                    after: StopCompletion::Restart,
                },
            )
        }
        Operation::Halt => {
            machine.set_desired(DesiredState::Halt);
            machine.state().live_generation().map_or(
                ControlEffect::ExitSupervisor { leave_child: false },
                |generation| ControlEffect::StopGroup {
                    generation,
                    after: StopCompletion::Halt,
                },
            )
        }
        Operation::Exit => {
            machine.set_desired(DesiredState::Exit);
            ControlEffect::ExitSupervisor {
                leave_child: machine.state().live_generation().is_some(),
            }
        }
        Operation::Signal => {
            let Some(generation) = machine.state().live_generation() else {
                return rejected(machine, ResponseCode::Invalid, "service has no live child");
            };
            let Some(signal) = request.signal else {
                return rejected(machine, ResponseCode::Invalid, "signal is missing");
            };
            ControlEffect::DeliverSignal {
                generation,
                scope: request.scope,
                signal,
            }
        }
    };
    ControlDecision {
        response: Response {
            code: ResponseCode::Ok,
            generation: machine.state().generation(),
            message: "accepted; awaiting lifecycle completion".to_owned(),
            status: None,
        },
        effect,
    }
}

fn status_decision(machine: &StateMachine, generation: Option<Generation>) -> ControlDecision {
    ControlDecision {
        response: Response {
            code: ResponseCode::Ok,
            generation,
            message: status_message(machine),
            status: Some(StatusSnapshot::from_machine(machine)),
        },
        effect: ControlEffect::None,
    }
}

fn start_effect(state: SupervisorState) -> ControlEffect {
    match state {
        SupervisorState::Down | SupervisorState::Failed(_) => ControlEffect::BeginStart,
        SupervisorState::WaitingCondition | SupervisorState::Backoff { .. } => {
            ControlEffect::CancelPending { start: true }
        }
        SupervisorState::Starting(_)
        | SupervisorState::Running(_)
        | SupervisorState::Ready(_)
        | SupervisorState::Paused { .. }
        | SupervisorState::Stopping(_)
        | SupervisorState::Completed(_)
        | SupervisorState::Initializing
        | SupervisorState::Exited => ControlEffect::None,
    }
}

fn stop_effect(state: SupervisorState, after: StopCompletion) -> ControlEffect {
    state.live_generation().map_or_else(
        || {
            if matches!(
                state,
                SupervisorState::WaitingCondition | SupervisorState::Backoff { .. }
            ) {
                ControlEffect::CancelPending { start: false }
            } else {
                ControlEffect::None
            }
        },
        |generation| ControlEffect::StopGroup { generation, after },
    )
}

fn rejected(machine: &StateMachine, code: ResponseCode, message: &str) -> ControlDecision {
    ControlDecision {
        response: Response {
            code,
            generation: machine.state().generation(),
            message: message.to_owned(),
            status: None,
        },
        effect: ControlEffect::None,
    }
}

fn status_message(machine: &StateMachine) -> String {
    format!(
        "desired={} state={}",
        desired_name(machine.desired()),
        state_name(machine.state())
    )
}

const fn desired_name(desired: DesiredState) -> &'static str {
    match desired {
        DesiredState::Up => "up",
        DesiredState::Down => "down",
        DesiredState::Once => "once",
        DesiredState::Halt => "halt",
        DesiredState::Exit => "exit",
    }
}

const fn state_name(state: SupervisorState) -> &'static str {
    match state {
        SupervisorState::Initializing => "initializing",
        SupervisorState::Down => "down",
        SupervisorState::WaitingCondition => "waiting-condition",
        SupervisorState::Starting(_) => "starting",
        SupervisorState::Running(_) => "running",
        SupervisorState::Ready(_) => "ready",
        SupervisorState::Paused { .. } => "paused",
        SupervisorState::Stopping(_) => "stopping",
        SupervisorState::Backoff { .. } => "backoff",
        SupervisorState::Completed(_) => "completed",
        SupervisorState::Failed(_) => "failed",
        SupervisorState::Exited => "exited",
    }
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

fn validate_request(request: &Request) -> Result<(), ProtocolError> {
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
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    if safe {
        Ok(())
    } else {
        Err(ProtocolError::UnsafeServiceName)
    }
}

/// Protocol decoding or validation error.
#[derive(Debug)]
pub enum ProtocolError {
    /// Frame exceeds [`MAX_FRAME_BYTES`].
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
    /// Service name exceeds [`MAX_SERVICE_NAME_BYTES`].
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

/// Failure while transferring one bounded frame.
#[derive(Debug)]
pub enum TransportError {
    /// Asynchronous stream I/O failed.
    Io(io::Error),
    /// Complete frame violated the control protocol.
    Protocol(ProtocolError),
    /// Peer did not complete the operation before the idle deadline.
    Timeout,
}

impl Display for TransportError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "control transport I/O failed: {error}"),
            Self::Protocol(error) => Display::fmt(error, formatter),
            Self::Timeout => formatter.write_str("control transport deadline exceeded"),
        }
    }
}

impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Timeout => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProtocolError> for TransportError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// Read exactly one request using the default idle deadline.
///
/// # Errors
///
/// Returns an error for I/O failure, timeout, oversized input, or an invalid
/// request frame.
pub async fn read_request<R>(reader: &mut R) -> Result<Request, TransportError>
where
    R: AsyncRead + Unpin,
{
    read_request_with_timeout(reader, CONTROL_IO_TIMEOUT).await
}

/// Read exactly one request using an explicit idle deadline.
///
/// # Errors
///
/// Returns an error for I/O failure, timeout, oversized input, or an invalid
/// request frame.
pub async fn read_request_with_timeout<R>(
    reader: &mut R,
    idle_timeout: Duration,
) -> Result<Request, TransportError>
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame_with_timeout(reader, idle_timeout).await?;
    Request::decode(&frame).map_err(TransportError::Protocol)
}

/// Write exactly one request using the default idle deadline.
///
/// # Errors
///
/// Returns an error for request encoding, I/O failure, or timeout.
pub async fn write_request<W>(writer: &mut W, request: &Request) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let frame = request.encode()?;
    write_frame_with_timeout(writer, &frame, CONTROL_IO_TIMEOUT).await
}

/// Read exactly one response using the default idle deadline.
///
/// # Errors
///
/// Returns an error for I/O failure, timeout, oversized input, or an invalid
/// response frame.
pub async fn read_response<R>(reader: &mut R) -> Result<Response, TransportError>
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame_with_timeout(reader, CONTROL_IO_TIMEOUT).await?;
    Response::decode(&frame).map_err(TransportError::Protocol)
}

/// Write exactly one response using the default idle deadline.
///
/// # Errors
///
/// Returns an error for response encoding, I/O failure, or timeout.
pub async fn write_response<W>(writer: &mut W, response: &Response) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let frame = response.encode()?;
    write_frame_with_timeout(writer, &frame, CONTROL_IO_TIMEOUT).await
}

/// Connect to one local supervisor and exchange exactly one bounded request.
///
/// # Errors
///
/// Returns a timeout, Unix-socket I/O, request encoding, or response protocol
/// failure. Peer authorization remains enforced by the supervisor.
#[cfg(unix)]
pub async fn exchange(path: &Path, request: &Request) -> Result<Response, TransportError> {
    let mut stream = timeout(CONTROL_IO_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| TransportError::Timeout)??;
    write_request(&mut stream, request).await?;
    read_response(&mut stream).await
}

async fn read_frame_with_timeout<R>(
    reader: &mut R,
    idle_timeout: Duration,
) -> Result<Vec<u8>, TransportError>
where
    R: AsyncRead + Unpin,
{
    timeout(idle_timeout, read_frame(reader))
        .await
        .map_err(|_| TransportError::Timeout)?
}

async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, TransportError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).await?;
    let length_bytes: [u8; 2] = header
        .get(HEADER_BYTES - 2..)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let payload_length = usize::from(u16::from_be_bytes(length_bytes));
    let frame_length = HEADER_BYTES
        .checked_add(payload_length)
        .ok_or(ProtocolError::FrameTooLarge(usize::MAX))?;
    if frame_length > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(frame_length).into());
    }
    let mut payload = vec![0_u8; payload_length];
    reader.read_exact(&mut payload).await?;
    let mut frame = Vec::with_capacity(frame_length);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

async fn write_frame_with_timeout<W>(
    writer: &mut W,
    frame: &[u8],
    idle_timeout: Duration,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    timeout(idle_timeout, async {
        writer.write_all(frame).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| TransportError::Timeout)??;
    Ok(())
}

/// Authenticated peer identity obtained from the Unix socket.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    /// Effective user ID of the connecting process.
    pub uid: u32,
    /// Effective group ID of the connecting process.
    pub gid: u32,
    /// Process ID when the operating system exposes it.
    pub pid: Option<i32>,
}

/// An authorized connection which occupies one bounded server slot.
#[cfg(unix)]
#[derive(Debug)]
pub struct AuthorizedConnection {
    stream: UnixStream,
    peer: PeerCredentials,
    _permit: OwnedSemaphorePermit,
}

#[cfg(unix)]
impl AuthorizedConnection {
    /// Credentials authenticated when this connection was accepted.
    #[must_use]
    pub const fn peer(&self) -> PeerCredentials {
        self.peer
    }

    /// Borrow the stream for bounded request and response transfer.
    pub const fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    /// Consume the authorization guard and return its stream.
    #[must_use]
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// Authenticated request forwarded to the single-owner supervisor event loop.
#[cfg(unix)]
#[derive(Debug)]
pub struct ControlCommand {
    request: Request,
    peer: PeerCredentials,
    response: oneshot::Sender<Response>,
}

#[cfg(unix)]
impl ControlCommand {
    /// Validated bounded request received from the peer.
    #[must_use]
    pub const fn request(&self) -> &Request {
        &self.request
    }

    /// Credentials authenticated before reading the request.
    #[must_use]
    pub const fn peer(&self) -> PeerCredentials {
        self.peer
    }

    /// Complete this request after lifecycle work and status publication.
    ///
    /// # Errors
    ///
    /// Returns the boxed response when the client disconnected before completion.
    pub fn respond(self, response: Response) -> Result<(), Box<Response>> {
        self.response.send(response).map_err(Box::new)
    }
}

/// Run the authenticated socket side of the control server.
///
/// Each accepted client is bounded by [`ControlListener`], frame deadlines, and
/// the supplied bounded command channel. The receiving supervisor event loop
/// remains the only owner of lifecycle state. Malformed/disconnected clients
/// are isolated to their connection task. Shutdown aborts and joins every
/// remaining client task before returning.
///
/// # Errors
///
/// Returns a fatal listener failure or a closed command receiver.
#[cfg(unix)]
pub async fn run_control_server(
    listener: Arc<ControlListener>,
    commands: mpsc::Sender<ControlCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), AcceptError> {
    let mut tasks = JoinSet::new();
    if *shutdown.borrow() {
        return Ok(());
    }
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            () = commands.closed() => return Err(AcceptError::ShuttingDown),
            accepted = listener.accept() => match accepted {
                Ok(connection) => {
                    let sender = commands.clone();
                    tasks.spawn(async move {
                        let _isolated_error = serve_control_connection(connection, sender).await;
                    });
                }
                Err(AcceptError::Timeout | AcceptError::PermissionDenied { .. }) => {}
                Err(error) => return Err(error),
            },
            Some(_completed) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

#[cfg(unix)]
async fn serve_control_connection(
    mut connection: AuthorizedConnection,
    commands: mpsc::Sender<ControlCommand>,
) -> Result<(), TransportError> {
    let request = read_request(connection.stream_mut()).await?;
    let peer = connection.peer();
    let (response, receiver) = oneshot::channel();
    commands
        .send(ControlCommand {
            request,
            peer,
            response,
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "supervisor control command receiver closed",
            )
        })?;
    let response = receiver.await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "supervisor dropped a control response",
        )
    })?;
    write_response(connection.stream_mut(), &response).await
}

/// Failure while accepting and authenticating a control connection.
#[cfg(unix)]
#[derive(Debug)]
pub enum AcceptError {
    /// Listener or peer-credential operation failed.
    Io(io::Error),
    /// No client slot or connection arrived before the deadline.
    Timeout,
    /// Semaphore was closed during shutdown.
    ShuttingDown,
    /// Peer is neither root nor the supervisor owner.
    PermissionDenied {
        /// Effective UID rejected by the server.
        uid: u32,
    },
}

#[cfg(unix)]
impl Display for AcceptError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "unable to accept control connection: {error}"),
            Self::Timeout => formatter.write_str("control accept deadline exceeded"),
            Self::ShuttingDown => formatter.write_str("control listener is shutting down"),
            Self::PermissionDenied { uid } => {
                write!(formatter, "control peer UID {uid} is not authorized")
            }
        }
    }
}

#[cfg(unix)]
impl Error for AcceptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Timeout | Self::ShuttingDown | Self::PermissionDenied { .. } => None,
        }
    }
}

#[cfg(unix)]
impl From<io::Error> for AcceptError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

/// Owned, permission-restricted, authenticated local control listener.
#[cfg(unix)]
#[derive(Debug)]
pub struct ControlListener {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    owner_uid: u32,
    clients: Arc<Semaphore>,
}

#[cfg(unix)]
impl ControlListener {
    /// Bind a new control socket without deleting or replacing an existing path.
    ///
    /// The path must be absolute, its parent must be canonical, owned by the
    /// effective account creating the socket, and not group/world writable.
    /// The resulting socket mode is `0600`.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, an existing entry, invalid client
    /// limits, binding failure, or permission/metadata failure.
    pub fn bind(path: &Path, max_clients: usize) -> io::Result<Self> {
        if max_clients == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "maximum control clients must be greater than zero",
            ));
        }
        validate_socket_parent(path)?;
        match fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "control socket path already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let listener = UnixListener::bind(path)?;
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            cleanup_created_socket(path);
            return Err(error);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                cleanup_created_socket(path);
                return Err(error);
            }
        };
        if !metadata.file_type().is_socket() {
            cleanup_created_socket(path);
            return Err(io::Error::other("new control socket path is not a socket"));
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "control socket has no parent")
        })?;
        let parent_metadata = fs::symlink_metadata(parent)?;
        if metadata.uid() != parent_metadata.uid() {
            cleanup_created_socket(path);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control socket and runtime directory owners differ",
            ));
        }

        Ok(Self {
            listener,
            path: path.to_owned(),
            identity: SocketIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            owner_uid: metadata.uid(),
            clients: Arc::new(Semaphore::new(max_clients)),
        })
    }

    /// Socket path owned by this listener.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Effective UID authorized in addition to root.
    #[must_use]
    pub const fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    /// Accept and authenticate a connection with the default idle deadline.
    ///
    /// # Errors
    ///
    /// Returns an error for timeout, shutdown, listener/credential failure, or
    /// an unauthorized peer.
    pub async fn accept(&self) -> Result<AuthorizedConnection, AcceptError> {
        self.accept_with_timeout(CONTROL_IO_TIMEOUT).await
    }

    /// Accept and authenticate a connection with an explicit idle deadline.
    ///
    /// # Errors
    ///
    /// Returns an error for timeout, shutdown, listener/credential failure, or
    /// an unauthorized peer.
    pub async fn accept_with_timeout(
        &self,
        idle_timeout: Duration,
    ) -> Result<AuthorizedConnection, AcceptError> {
        let permit = timeout(idle_timeout, Arc::clone(&self.clients).acquire_owned())
            .await
            .map_err(|_| AcceptError::Timeout)?
            .map_err(|_| AcceptError::ShuttingDown)?;
        let (stream, _) = timeout(idle_timeout, self.listener.accept())
            .await
            .map_err(|_| AcceptError::Timeout)??;
        let credentials = stream.peer_cred()?;
        let peer = PeerCredentials {
            uid: credentials.uid(),
            gid: credentials.gid(),
            pid: credentials.pid(),
        };
        if !peer_is_authorized(peer.uid, self.owner_uid) {
            return Err(AcceptError::PermissionDenied { uid: peer.uid });
        }
        Ok(AuthorizedConnection {
            stream,
            peer,
            _permit: permit,
        })
    }
}

#[cfg(unix)]
impl Drop for ControlListener {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.identity.device
            && metadata.ino() == self.identity.inode
        {
            let _ignored = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn validate_socket_parent(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket path must be an absolute file path",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "control socket has no parent")
    })?;
    let canonical = fs::canonicalize(parent)?;
    if canonical != parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket parent must be canonical and contain no symlink",
        ));
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket parent is not a real directory",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control socket parent must not be group or world writable",
        ));
    }
    Ok(())
}

#[cfg(unix)]
const fn peer_is_authorized(peer_uid: u32, owner_uid: u32) -> bool {
    peer_uid == 0 || peer_uid == owner_uid
}

#[cfg(unix)]
fn cleanup_created_socket(path: &Path) {
    let _ignored = fs::remove_file(path);
}

struct Cursor<'a> {
    remaining: &'a [u8],
}

impl<'a> Cursor<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(N)
            .ok_or(ProtocolError::Truncated)?;
        self.remaining = remaining;
        value.try_into().map_err(|_| ProtocolError::Truncated)
    }

    fn byte(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take::<1>()?.into_iter().next().unwrap_or_default())
    }

    fn bytes(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(count)
            .ok_or(ProtocolError::Truncated)?;
        self.remaining = remaining;
        Ok(value)
    }

    const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    const fn len(&self) -> usize {
        self.remaining.len()
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, time::Duration};

    use tokio::io::{AsyncWriteExt, duplex};

    #[cfg(unix)]
    use std::{
        fs,
        os::unix::fs::{MetadataExt, PermissionsExt},
        path::{Path, PathBuf},
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
    };
    #[cfg(unix)]
    use tokio::net::UnixStream;
    #[cfg(unix)]
    use tokio::sync::{mpsc, watch};

    #[cfg(unix)]
    use super::{AcceptError, ControlListener, peer_is_authorized, run_control_server};
    use super::{
        ControlEffect, GenerationMatch, MAX_FRAME_BYTES, Operation, PROTOCOL_VERSION,
        ProtocolError, Request, Response, ResponseCode, Signal, SignalScope, StopCompletion,
        TransportError, decide_request, read_request, read_request_with_timeout, read_response,
        write_request, write_response,
    };
    use crate::status::{
        LastResult, LoggerStatus, MAX_STATUS_ARGUMENTS, ReadinessStatus, ServiceState,
        StatusSnapshot,
    };
    use crate::supervisor::{
        DesiredState, FailureReason, Generation, RestartDecision, StateMachine, SupervisorState,
    };

    #[cfg(unix)]
    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[cfg(unix)]
    struct TestDirectory(PathBuf);

    #[cfg(unix)]
    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = Path::new("/tmp").join(format!(
                "immortal-control-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            Ok(Self(fs::canonicalize(path)?))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    #[cfg(unix)]
    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn all_signal_names_round_trip() -> Result<(), Box<dyn Error>> {
        for name in [
            "usr1", "usr2", "alrm", "cont", "hup", "int", "kill", "ttin", "ttou", "quit", "stop",
            "term", "winch",
        ] {
            let signal = Signal::from_name(name).ok_or(ProtocolError::UnknownSignal(u8::MAX))?;
            let request = Request {
                operation: Operation::Signal,
                service: "api.worker-1".to_owned(),
                expected_generation: GenerationMatch::Exact(Generation::from_protocol(42)),
                scope: SignalScope::Group,
                signal: Some(signal),
            };
            assert_eq!(Request::decode(&request.encode()?)?, request);
            assert_eq!(signal.name(), name);
        }
        Ok(())
    }

    #[test]
    fn status_is_valid_before_the_first_child() {
        let mut machine = StateMachine::default();
        let decision = decide_request(
            "api",
            &mut machine,
            &Request {
                operation: Operation::Status,
                service: "api".to_owned(),
                expected_generation: GenerationMatch::Any,
                scope: SignalScope::Main,
                signal: None,
            },
        );
        assert_eq!(decision.response.code, ResponseCode::Ok);
        assert_eq!(decision.response.generation, None);
        assert_eq!(decision.response.message, "desired=up state=down");
        assert_eq!(decision.effect, ControlEffect::None);
    }

    #[test]
    fn restart_is_bound_to_the_exact_live_generation() -> Result<(), Box<dyn Error>> {
        let mut machine = ready_machine()?;
        let generation = machine
            .state()
            .live_generation()
            .ok_or("ready machine has no generation")?;
        let stale = decide_request(
            "api",
            &mut machine,
            &Request {
                operation: Operation::Restart,
                service: "api".to_owned(),
                expected_generation: GenerationMatch::NoChild,
                scope: SignalScope::Main,
                signal: None,
            },
        );
        assert_eq!(stale.response.code, ResponseCode::Conflict);
        assert_eq!(stale.effect, ControlEffect::None);

        let accepted = decide_request(
            "api",
            &mut machine,
            &Request {
                operation: Operation::Restart,
                service: "api".to_owned(),
                expected_generation: GenerationMatch::Exact(generation),
                scope: SignalScope::Main,
                signal: None,
            },
        );
        assert_eq!(accepted.response.code, ResponseCode::Ok);
        assert_eq!(machine.desired(), DesiredState::Up);
        assert_eq!(
            accepted.effect,
            ControlEffect::StopGroup {
                generation,
                after: StopCompletion::Restart,
            }
        );
        Ok(())
    }

    #[test]
    fn signal_requires_a_live_generation() {
        let mut machine = StateMachine::default();
        let decision = decide_request(
            "api",
            &mut machine,
            &Request {
                operation: Operation::Signal,
                service: "api".to_owned(),
                expected_generation: GenerationMatch::NoChild,
                scope: SignalScope::Main,
                signal: Some(Signal::Hangup),
            },
        );
        assert_eq!(decision.response.code, ResponseCode::Invalid);
        assert_eq!(decision.effect, ControlEffect::None);
    }

    #[test]
    fn manual_restart_resets_configured_failure() -> Result<(), Box<dyn Error>> {
        let mut machine = StateMachine::default();
        machine.begin_start()?;
        let generation = machine.preconditions_ready()?;
        machine.child_started(generation)?;
        machine.child_reaped(generation, RestartDecision::Fail(FailureReason::RetryLimit))?;
        assert!(matches!(machine.state(), SupervisorState::Failed(_)));

        let decision = decide_request(
            "api",
            &mut machine,
            &Request {
                operation: Operation::Restart,
                service: "api".to_owned(),
                expected_generation: GenerationMatch::NoChild,
                scope: SignalScope::Main,
                signal: None,
            },
        );
        assert_eq!(machine.state(), SupervisorState::Down);
        assert_eq!(machine.desired(), DesiredState::Up);
        assert_eq!(decision.effect, ControlEffect::BeginStart);
        Ok(())
    }

    fn ready_machine() -> Result<StateMachine, Box<dyn Error>> {
        let mut machine = StateMachine::default();
        machine.begin_start()?;
        let generation = machine.preconditions_ready()?;
        machine.child_started(generation)?;
        machine.child_ready(generation)?;
        Ok(machine)
    }

    #[test]
    fn every_lifecycle_operation_round_trips() -> Result<(), Box<dyn Error>> {
        for operation in [
            Operation::Status,
            Operation::Start,
            Operation::Stop,
            Operation::Restart,
            Operation::Once,
            Operation::Exit,
            Operation::Halt,
        ] {
            let request = Request {
                operation,
                service: "api".to_owned(),
                expected_generation: if operation == Operation::Status {
                    GenerationMatch::Any
                } else {
                    GenerationMatch::NoChild
                },
                scope: SignalScope::Main,
                signal: None,
            };
            assert_eq!(Request::decode(&request.encode()?)?, request);
        }
        Ok(())
    }

    #[test]
    fn rejects_oversized_truncated_and_trailing_frames() -> Result<(), Box<dyn Error>> {
        assert!(matches!(
            Request::decode(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(ProtocolError::FrameTooLarge(_))
        ));
        assert!(matches!(
            Request::decode(b"IMMO"),
            Err(ProtocolError::Truncated)
        ));
        let mut valid = Request {
            operation: Operation::Status,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        }
        .encode()?;
        valid.push(0);
        assert!(matches!(
            Request::decode(&valid),
            Err(ProtocolError::TrailingBytes)
        ));
        Ok(())
    }

    #[test]
    fn rejects_unknown_version_operation_scope_and_signal() -> Result<(), Box<dyn Error>> {
        let request = Request {
            operation: Operation::Signal,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: Some(Signal::Hangup),
        };
        let frame = request.encode()?;

        let mut version = frame.clone();
        if let Some(byte) = version.get_mut(4) {
            *byte = PROTOCOL_VERSION.saturating_add(1);
        }
        assert!(matches!(
            Request::decode(&version),
            Err(ProtocolError::UnsupportedVersion(_))
        ));

        let mut operation = frame.clone();
        if let Some(byte) = operation.get_mut(5) {
            *byte = u8::MAX;
        }
        assert!(matches!(
            Request::decode(&operation),
            Err(ProtocolError::UnknownOperation(_))
        ));

        let mut scope = frame.clone();
        if let Some(byte) = scope.get_mut(6) {
            *byte = u8::MAX;
        }
        assert!(matches!(
            Request::decode(&scope),
            Err(ProtocolError::UnknownScope(_))
        ));

        let mut signal = frame;
        if let Some(byte) = signal.get_mut(7) {
            *byte = u8::MAX;
        }
        assert!(matches!(
            Request::decode(&signal),
            Err(ProtocolError::UnknownSignal(_))
        ));
        Ok(())
    }

    #[test]
    fn rejects_unsafe_names_and_inconsistent_signal_fields() {
        let unsafe_name = Request {
            operation: Operation::Status,
            service: "../api".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        };
        assert!(matches!(
            unsafe_name.encode(),
            Err(ProtocolError::UnsafeServiceName)
        ));

        let missing = Request {
            operation: Operation::Signal,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        };
        assert!(matches!(
            missing.encode(),
            Err(ProtocolError::MissingSignal)
        ));

        let unguarded = Request {
            operation: Operation::Restart,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        };
        assert!(matches!(
            unguarded.encode(),
            Err(ProtocolError::MissingGenerationMatch)
        ));
    }

    #[test]
    fn bounded_response_round_trips() -> Result<(), Box<dyn Error>> {
        for code in [
            ResponseCode::Ok,
            ResponseCode::NotFound,
            ResponseCode::PermissionDenied,
            ResponseCode::Conflict,
            ResponseCode::Invalid,
            ResponseCode::Internal,
        ] {
            let response = Response {
                code,
                generation: Some(Generation::from_protocol(7)),
                message: "state=ready".to_owned(),
                status: None,
            };
            assert_eq!(Response::decode(&response.encode()?)?, response);
        }
        Ok(())
    }

    #[test]
    fn typed_status_payload_round_trips_every_field() -> Result<(), Box<dyn Error>> {
        let status = StatusSnapshot {
            supervisor_pid: Some(101),
            main_pid: Some(202),
            desired: DesiredState::Up,
            state: ServiceState::Backoff,
            readiness: ReadinessStatus::TimedOut,
            uptime_seconds: Some(17),
            down_seconds: Some(3),
            starts: 9,
            failures: 4,
            last_result: Some(LastResult::Signaled(9)),
            backoff_seconds: Some(8),
            logger: LoggerStatus::Failed,
            command: vec!["/usr/bin/api".to_owned(), "argument with spaces".to_owned()],
        };
        let response = Response {
            code: ResponseCode::Ok,
            generation: Some(Generation::from_protocol(7)),
            message: "status".to_owned(),
            status: Some(status),
        };
        assert_eq!(Response::decode(&response.encode()?)?, response);
        Ok(())
    }

    #[test]
    fn typed_status_rejects_unknown_payload_and_unbounded_arguments() -> Result<(), Box<dyn Error>>
    {
        let response = Response {
            code: ResponseCode::Ok,
            generation: None,
            message: String::new(),
            status: None,
        };
        let mut unknown = response.encode()?;
        if let Some(payload) = unknown.get_mut(6) {
            *payload = u8::MAX;
        }
        assert!(matches!(
            Response::decode(&unknown),
            Err(ProtocolError::UnknownResponsePayload(_))
        ));

        let mut status = StatusSnapshot::from_machine(&StateMachine::default());
        status.command = vec!["argument".to_owned(); MAX_STATUS_ARGUMENTS + 1];
        assert!(matches!(
            (Response {
                code: ResponseCode::Ok,
                generation: None,
                message: String::new(),
                status: Some(status),
            })
            .encode(),
            Err(ProtocolError::TooManyStatusArguments)
        ));
        Ok(())
    }

    #[test]
    fn response_rejects_oversized_and_malformed_data() {
        let oversized = Response {
            code: ResponseCode::Ok,
            generation: None,
            message: "x".repeat(MAX_FRAME_BYTES),
            status: None,
        };
        assert!(matches!(
            oversized.encode(),
            Err(ProtocolError::MessageTooLong | ProtocolError::FrameTooLarge(_))
        ));
        assert!(matches!(
            Response::decode(b"IMMO"),
            Err(ProtocolError::Truncated)
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_transport_round_trips_one_request_and_response() -> Result<(), Box<dyn Error>> {
        let request = Request {
            operation: Operation::Signal,
            service: "worker".to_owned(),
            expected_generation: GenerationMatch::Exact(Generation::from_protocol(9)),
            scope: SignalScope::Group,
            signal: Some(Signal::Terminate),
        };
        let response = Response {
            code: ResponseCode::Ok,
            generation: Some(Generation::from_protocol(9)),
            message: "signal accepted".to_owned(),
            status: None,
        };
        let (mut client, mut server) = duplex(1024);

        write_request(&mut client, &request).await?;
        assert_eq!(read_request(&mut server).await?, request);
        write_response(&mut server, &response).await?;
        assert_eq!(read_response(&mut client).await?, response);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_transport_times_out_idle_clients() {
        let (_writer, mut reader) = duplex(64);
        let result = read_request_with_timeout(&mut reader, Duration::from_millis(1)).await;
        assert!(matches!(result, Err(TransportError::Timeout)));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_transport_rejects_declared_oversized_frames_before_payload_read()
    -> Result<(), Box<dyn Error>> {
        let request = Request {
            operation: Operation::Status,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        };
        let mut header = request.encode()?;
        header.truncate(super::HEADER_BYTES);
        let length = u16::MAX.to_be_bytes();
        if let Some(target) = header.get_mut(super::HEADER_BYTES - 2..) {
            target.copy_from_slice(&length);
        }
        let (mut writer, mut reader) = duplex(64);
        writer.write_all(&header).await?;

        let result = read_request(&mut reader).await;
        assert!(matches!(
            result,
            Err(TransportError::Protocol(ProtocolError::FrameTooLarge(_)))
        ));
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn async_transport_reports_truncated_frames() -> Result<(), Box<dyn Error>> {
        let (mut writer, mut reader) = duplex(64);
        writer.write_all(b"IMMO").await?;
        writer.shutdown().await?;

        let result = read_request(&mut reader).await;
        assert!(matches!(result, Err(TransportError::Io(_))));
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn owned_listener_restricts_mode_and_authenticates_owner() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("control.sock");
        let listener = ControlListener::bind(&path, 2)?;
        let mut client = UnixStream::connect(&path).await?;
        let mut connection = listener.accept().await?;

        assert_eq!(connection.peer().uid, listener.owner_uid());
        assert_eq!(fs::symlink_metadata(&path)?.mode() & 0o777, 0o600);
        let request = Request {
            operation: Operation::Status,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        };
        write_request(&mut client, &request).await?;
        assert_eq!(read_request(connection.stream_mut()).await?, request);

        drop(connection);
        drop(client);
        drop(listener);
        assert!(!path.exists());
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn listener_bounds_active_clients() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("control.sock");
        let listener = ControlListener::bind(&path, 1)?;
        let first_client = UnixStream::connect(&path).await?;
        let first = listener.accept().await?;
        let second_client = UnixStream::connect(&path).await?;

        assert!(matches!(
            listener.accept_with_timeout(Duration::from_millis(1)).await,
            Err(AcceptError::Timeout)
        ));
        drop(first);
        let second = listener.accept().await?;

        drop(second);
        drop(second_client);
        drop(first_client);
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn server_loop_isolates_bad_clients_and_dispatches_authenticated_requests()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("immortal.sock");
        let listener = Arc::new(ControlListener::bind(&path, 2)?);
        let owner_uid = listener.owner_uid();
        let (sender, mut commands) = mpsc::channel(1);
        let (shutdown_sender, shutdown) = watch::channel(false);
        let mut server = tokio::spawn(run_control_server(Arc::clone(&listener), sender, shutdown));

        let mut malformed = UnixStream::connect(&path).await?;
        malformed.write_all(b"bad").await?;
        drop(malformed);

        let request = Request {
            operation: Operation::Status,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        };
        let client = async {
            let mut stream = UnixStream::connect(&path).await?;
            write_request(&mut stream, &request).await?;
            let response = read_response(&mut stream).await?;
            Ok::<Response, Box<dyn Error>>(response)
        };
        let dispatch = async {
            let command = tokio::select! {
                command = commands.recv() => command.ok_or("control command missing")?,
                result = &mut server => {
                    return Err(format!("control server stopped before dispatch: {result:?}").into());
                }
            };
            assert_eq!(command.request(), &request);
            assert!(command.peer().uid == 0 || command.peer().uid == owner_uid);
            command
                .respond(Response {
                    code: ResponseCode::Ok,
                    generation: None,
                    message: "dispatched".to_owned(),
                    status: Some(StatusSnapshot::from_machine(&StateMachine::default())),
                })
                .map_err(|_| "client disconnected before response")?;
            Ok::<(), Box<dyn Error>>(())
        };
        let (client_result, dispatch_result) = tokio::join!(client, dispatch);
        dispatch_result?;
        assert_eq!(client_result?.message, "dispatched");

        shutdown_sender.send(true)?;
        server.await??;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn listener_refuses_unsafe_or_existing_paths() -> Result<(), Box<dyn Error>> {
        assert!(ControlListener::bind(Path::new("relative.sock"), 1).is_err());
        let directory = TestDirectory::new()?;
        let path = directory.path().join("control.sock");
        fs::write(&path, b"do not replace")?;
        assert!(ControlListener::bind(&path, 1).is_err());
        assert_eq!(fs::read(&path)?, b"do not replace");
        Ok(())
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn listener_cleanup_never_removes_a_replacement() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("control.sock");
        let listener = ControlListener::bind(&path, 1)?;
        fs::remove_file(&path)?;
        fs::write(&path, b"replacement")?;

        drop(listener);
        assert_eq!(fs::read(&path)?, b"replacement");
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn peer_policy_allows_only_root_or_owner() {
        assert!(peer_is_authorized(0, 1000));
        assert!(peer_is_authorized(1000, 1000));
        assert!(!peer_is_authorized(1001, 1000));
    }
}
