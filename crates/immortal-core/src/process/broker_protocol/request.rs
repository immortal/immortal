//! Bounded request messages sent from the supervisor to the process broker.
//!
//! [`BrokerRequest`] is the exhaustive set of operations the single-threaded
//! broker accepts: spawning a service or auxiliary task, signaling or
//! detaching a generation, closing logger inputs, and shutting down. Each
//! variant's `encode`/`decode` pair is the only place request bytes are
//! produced or consumed, keeping the wire format and its validation
//! precedence in one place per message kind.

use std::time::Duration;

use crate::supervisor::Generation;

use super::codec::{
    decode_command, decode_generation, decode_optional_timeout, decode_timeout, encode_command,
    encode_generation, encode_optional_timeout, encode_timeout,
};
use super::error::BrokerProtocolError;
use super::framing::{Cursor, decode_boolean, decode_frame, encode_frame};
use super::{BrokerLoggerId, ProcessCommand, ProcessSignal};

const REQUEST_SPAWN: u8 = 1;
const REQUEST_SIGNAL: u8 = 2;
const REQUEST_SHUTDOWN: u8 = 3;
const REQUEST_DETACH: u8 = 4;
const REQUEST_SPAWN_LOGGER: u8 = 5;
const REQUEST_CLOSE_LOGGER_INPUTS: u8 = 6;

/// Logical target resolved against the broker's currently owned generation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::process) enum BrokerSignalTarget {
    Process,
    Group,
}

/// Request sent from the Tokio supervisor to its single-threaded broker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::process) enum BrokerRequest {
    Spawn {
        generation: Generation,
        command: ProcessCommand,
        startup_timeout: Duration,
        readiness_timeout: Option<Duration>,
        lifetime_tracking: bool,
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
    pub(in crate::process) fn encode(&self) -> Result<Vec<u8>, BrokerProtocolError> {
        let mut payload = Vec::new();
        let kind = match self {
            Self::Spawn {
                generation,
                command,
                startup_timeout,
                readiness_timeout,
                lifetime_tracking,
            } => {
                encode_generation(*generation, &mut payload);
                encode_timeout(*startup_timeout, &mut payload)?;
                encode_optional_timeout(*readiness_timeout, &mut payload)?;
                payload.push(u8::from(*lifetime_tracking));
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

    pub(in crate::process) fn decode(frame: &[u8]) -> Result<Self, BrokerProtocolError> {
        let (kind, payload) = decode_frame(frame)?;
        let mut cursor = Cursor::new(payload);
        let request = match kind {
            REQUEST_SPAWN => Self::Spawn {
                generation: decode_generation(&mut cursor)?,
                startup_timeout: decode_timeout(&mut cursor)?,
                readiness_timeout: decode_optional_timeout(&mut cursor)?,
                lifetime_tracking: decode_boolean(&mut cursor)?,
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

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::ffi::OsString;
    use std::io;
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;
    use std::time::Duration;

    use crate::process::{
        BrokerLoggerId, ProcessCommand, ProcessCredentials, ProcessEnvironment, ProcessSignal,
        SupplementaryGroups,
    };
    use crate::supervisor::Generation;

    use super::super::error::BrokerProtocolError;
    use super::{BrokerRequest, BrokerSignalTarget};

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
            lifetime_tracking: true,
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
            lifetime_tracking: false,
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
            lifetime_tracking: false,
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

        let mut frame = spawn.encode()?;
        *frame
            .get_mut(34)
            .ok_or_else(|| io::Error::other("missing lifetime-tracking byte"))? = 2;
        assert_eq!(
            BrokerRequest::decode(&frame),
            Err(BrokerProtocolError::InvalidBoolean)
        );
        Ok(())
    }
}
