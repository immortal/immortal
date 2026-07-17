//! Bounded event messages sent from the process broker to the supervisor.
//!
//! [`BrokerEvent`] is the exhaustive set of observations the broker reports
//! back over the wire: generation lifecycle transitions, child exits,
//! signal delivery outcomes, readiness and lifetime observations, and
//! shutdown completion. Each variant's `encode`/`decode` pair is the only
//! place event bytes are produced or consumed.

use super::codec::{
    decode_child_event, decode_generation, decode_optional_error, decode_optional_process,
    decode_process, encode_child_event, encode_generation, encode_optional_error,
    encode_optional_process, encode_process, spawn_failure_code, spawn_failure_from_code,
    spawn_stage_code, spawn_stage_from_code,
};
use super::error::BrokerProtocolError;
use super::framing::{Cursor, decode_frame, encode_frame};
use super::{ChildEvent, ProcessGroupId, ProcessId, SpawnFailure, SpawnStage};
use crate::supervisor::Generation;

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
const EVENT_LIFETIME_CLOSED: u8 = 14;
const EVENT_LIFETIME_FAILED: u8 = 15;
const EVENT_CONTAINMENT_FAILED: u8 = 16;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::process) enum BrokerReadinessFailure {
    Timeout,
    Descriptor,
    InvalidToken,
}

/// Event sent from the process broker to the Tokio supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::process) enum BrokerEvent {
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
    LifetimeClosed {
        generation: Generation,
    },
    LifetimeFailed {
        generation: Generation,
    },
    ContainmentFailed {
        generation: Generation,
    },
    LoggerInputsClosed,
    ShutdownComplete,
    ShutdownFailed {
        os_error: Option<i32>,
    },
}

impl BrokerEvent {
    pub(in crate::process) fn encode(&self) -> Result<Vec<u8>, BrokerProtocolError> {
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
            Self::LifetimeClosed { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_LIFETIME_CLOSED
            }
            Self::LifetimeFailed { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_LIFETIME_FAILED
            }
            Self::ContainmentFailed { generation } => {
                encode_generation(*generation, &mut payload);
                EVENT_CONTAINMENT_FAILED
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

    pub(in crate::process) fn decode(frame: &[u8]) -> Result<Self, BrokerProtocolError> {
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
            EVENT_LIFETIME_CLOSED => Self::LifetimeClosed {
                generation: decode_generation(&mut cursor)?,
            },
            EVENT_LIFETIME_FAILED => Self::LifetimeFailed {
                generation: decode_generation(&mut cursor)?,
            },
            EVENT_CONTAINMENT_FAILED => Self::ContainmentFailed {
                generation: decode_generation(&mut cursor)?,
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

fn encode_group(group: ProcessGroupId, payload: &mut Vec<u8>) {
    payload.extend_from_slice(&group.get().to_be_bytes());
}

fn decode_group(cursor: &mut Cursor<'_>) -> Result<ProcessGroupId, BrokerProtocolError> {
    ProcessGroupId::new(i32::from_be_bytes(cursor.take::<4>()?))
        .ok_or(BrokerProtocolError::InvalidProcessGroupId)
}

#[cfg(test)]
mod tests {
    use std::error::Error;

    use crate::process::{ChildEvent, ProcessGroupId, ProcessId, SpawnFailure, SpawnStage};
    use crate::supervisor::Generation;

    use super::{BrokerEvent, BrokerReadinessFailure};

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
            BrokerEvent::LifetimeClosed { generation },
            BrokerEvent::LifetimeFailed { generation },
            BrokerEvent::ContainmentFailed { generation },
            BrokerEvent::LoggerInputsClosed,
            BrokerEvent::ShutdownComplete,
            BrokerEvent::ShutdownFailed { os_error: None },
        ] {
            assert_eq!(BrokerEvent::decode(&event.encode()?)?, event);
        }
        Ok(())
    }
}
