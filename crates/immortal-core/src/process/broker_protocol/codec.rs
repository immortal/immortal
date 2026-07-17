//! Wire codecs for command, credential, and scalar identifier fields.
//!
//! These encoders and decoders convert the domain types shared with the
//! supervisor (`ProcessCommand`, `ProcessCredentials`, `Generation`,
//! `ProcessId`, `ChildEvent`, `SpawnStage`, `SpawnFailure`) to and from
//! bounded byte sequences. Every decoder rejects out-of-range, duplicate, or
//! structurally invalid input with a typed [`BrokerProtocolError`] instead of
//! panicking, and every bounded collection (arguments, environment entries,
//! supplementary groups) is checked against a fixed limit before it is
//! materialized.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::time::Duration;

use crate::supervisor::Generation;

use super::error::BrokerProtocolError;
use super::framing::{Cursor, MAX_FIELD_BYTES};
use super::{
    ChildEvent, ProcessCommand, ProcessCredentials, ProcessId, SpawnFailure, SpawnStage,
    SupplementaryGroups,
};

pub(super) const MAX_ARGUMENTS: usize = 4_096;
pub(super) const MAX_ENVIRONMENT: usize = 4_096;
pub(super) const MAX_SUPPLEMENTARY_GROUPS: usize = 65_536;
const MAX_STARTUP_TIMEOUT: Duration = Duration::from_mins(5);
const MAX_READINESS_TIMEOUT: Duration = Duration::from_hours(24);
const CHILD_EXITED: u8 = 1;
const CHILD_SIGNALED: u8 = 2;
const CHILD_STOPPED: u8 = 3;
const CHILD_CONTINUED: u8 = 4;

pub(super) fn encode_command(
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

pub(super) fn decode_command(
    cursor: &mut Cursor<'_>,
) -> Result<ProcessCommand, BrokerProtocolError> {
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

pub(super) fn encode_timeout(
    timeout: Duration,
    payload: &mut Vec<u8>,
) -> Result<(), BrokerProtocolError> {
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

pub(super) fn decode_timeout(cursor: &mut Cursor<'_>) -> Result<Duration, BrokerProtocolError> {
    let timeout = Duration::from_millis(u64::from_be_bytes(cursor.take::<8>()?));
    if timeout.is_zero() || timeout > MAX_STARTUP_TIMEOUT {
        return Err(BrokerProtocolError::InvalidStartupTimeout);
    }
    Ok(timeout)
}

pub(super) fn encode_optional_timeout(
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

pub(super) fn decode_optional_timeout(
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

pub(super) fn encode_generation(generation: Generation, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&generation.get().to_be_bytes());
}

pub(super) fn decode_generation(
    cursor: &mut Cursor<'_>,
) -> Result<Generation, BrokerProtocolError> {
    Generation::new(u64::from_be_bytes(cursor.take::<8>()?))
        .ok_or(BrokerProtocolError::InvalidGeneration)
}

pub(super) fn encode_process(process: ProcessId, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&process.get().to_be_bytes());
}

pub(super) fn decode_process(cursor: &mut Cursor<'_>) -> Result<ProcessId, BrokerProtocolError> {
    ProcessId::new(i32::from_be_bytes(cursor.take::<4>()?))
        .ok_or(BrokerProtocolError::InvalidProcessId)
}

pub(super) fn encode_optional_process(process: Option<ProcessId>, payload: &mut Vec<u8>) {
    match process {
        Some(process) => {
            payload.push(1);
            encode_process(process, payload);
        }
        None => payload.push(0),
    }
}

pub(super) fn decode_optional_process(
    cursor: &mut Cursor<'_>,
) -> Result<Option<ProcessId>, BrokerProtocolError> {
    match cursor.byte()? {
        0 => Ok(None),
        1 => decode_process(cursor).map(Some),
        _ => Err(BrokerProtocolError::InvalidOptionalValue),
    }
}

pub(super) fn encode_optional_error(error: Option<i32>, payload: &mut Vec<u8>) {
    match error {
        Some(error) => {
            payload.push(1);
            payload.extend_from_slice(&error.to_be_bytes());
        }
        None => payload.push(0),
    }
}

pub(super) fn decode_optional_error(
    cursor: &mut Cursor<'_>,
) -> Result<Option<i32>, BrokerProtocolError> {
    match cursor.byte()? {
        0 => Ok(None),
        1 => Ok(Some(i32::from_be_bytes(cursor.take::<4>()?))),
        _ => Err(BrokerProtocolError::InvalidOptionalValue),
    }
}

pub(super) fn encode_child_event(event: ChildEvent, payload: &mut Vec<u8>) {
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

pub(super) fn decode_child_event(
    cursor: &mut Cursor<'_>,
) -> Result<ChildEvent, BrokerProtocolError> {
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

pub(super) const fn spawn_stage_code(stage: SpawnStage) -> u8 {
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

pub(super) fn spawn_stage_from_code(code: u8) -> Result<SpawnStage, BrokerProtocolError> {
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

pub(super) const fn spawn_failure_code(failure: SpawnFailure) -> u8 {
    match failure {
        SpawnFailure::OperatingSystem => 1,
        SpawnFailure::TimedOut => 2,
        SpawnFailure::InvalidHandshake => 3,
        SpawnFailure::InvalidForkContract => 4,
    }
}

pub(super) fn spawn_failure_from_code(code: u8) -> Result<SpawnFailure, BrokerProtocolError> {
    match code {
        1 => Ok(SpawnFailure::OperatingSystem),
        2 => Ok(SpawnFailure::TimedOut),
        3 => Ok(SpawnFailure::InvalidHandshake),
        4 => Ok(SpawnFailure::InvalidForkContract),
        _ => Err(BrokerProtocolError::InvalidSpawnFailure),
    }
}
