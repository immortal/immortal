//! Private, bounded wire contract between one supervisor and its process broker.
//!
//! This facade only declares the child modules that own one cohesive part of
//! the contract and re-exports the exact surface `super::broker` already
//! depended on before this module was split:
//!
//! - `framing` owns the ten-byte frame header, the declared-length lookup,
//!   and the bounded cursor payload reader shared by every codec.
//! - `codec` converts `ProcessCommand`/`ProcessCredentials` and the scalar
//!   identifier fields (`Generation`, `ProcessId`, `ProcessGroupId`,
//!   `ChildEvent`, `SpawnStage`, `SpawnFailure`) to and from bounded bytes.
//! - `request` and `event` each own one message direction's variant list
//!   and its `encode`/`decode` pair.
//! - `error` owns the exhaustive `BrokerProtocolError` returned by every
//!   codec in this contract.
//!
//! No behavior, wire byte, or validation precedence changed when this
//! module was split; only the ownership boundaries between files did.

mod codec;
mod error;
mod event;
mod framing;
mod request;

use super::{
    BrokerLoggerId, ChildEvent, ProcessCommand, ProcessCredentials, ProcessGroupId, ProcessId,
    ProcessSignal, SpawnFailure, SpawnStage, SupplementaryGroups,
};

pub(super) use self::error::BrokerProtocolError;
pub(super) use self::event::{BrokerEvent, BrokerReadinessFailure};
pub(super) use self::framing::{HEADER_BYTES, MAX_FRAME_BYTES, declared_frame_length};
pub(super) use self::request::{BrokerRequest, BrokerSignalTarget};
