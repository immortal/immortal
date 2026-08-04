//! Bounded, versioned local control protocol.
//!
//! The privileged supervisor accepts a small binary request vocabulary. JSON
//! formatting belongs to `immortalctl`, after the server response has crossed
//! the authenticated Unix-socket boundary.
//!
//! This module is a thin facade over focused children: `operation` owns the
//! request-intent enums and public protocol limits, `message` owns request and
//! response models plus their bounded codecs, `decision` owns pure supervisor
//! intent validation, `transport` owns asynchronous frame I/O and timeouts,
//! `server` owns the Unix-socket listener and peer authorization, and `wire`
//! owns shared framing constants and cursor state. Every child is private; the
//! public items below are re-exported so `immortal_core::control` remains the
//! single canonical path.

mod decision;
mod error;
mod message;
mod operation;
#[cfg(unix)]
mod server;
mod transport;
mod wire;

pub use crate::service_name::MAX_SERVICE_NAME_BYTES;

pub use self::decision::{ControlDecision, ControlEffect, StopCompletion, decide_request};
pub use self::error::ProtocolError;
pub use self::message::{Request, Response, ResponseCode};
pub use self::operation::{
    CONTROL_IO_TIMEOUT, DEFAULT_MAX_CONTROL_CLIENTS, GenerationMatch, MAX_FRAME_BYTES, Operation,
    PROTOCOL_VERSION, Signal, SignalScope,
};
#[cfg(unix)]
pub use self::server::{
    AcceptError, AuthorizedConnection, ControlCommand, ControlListener, PeerCredentials,
    run_control_server,
};
#[cfg(unix)]
pub use self::transport::exchange;
pub use self::transport::{
    TransportError, read_request, read_request_with_timeout, read_response, write_request,
    write_response,
};

#[cfg(all(test, unix))]
pub(in crate::control) use self::server::{
    accept_error_is_exhaustion, accept_error_is_transient, peer_is_authorized,
};
#[cfg(test)]
pub(in crate::control) use self::wire::HEADER_BYTES;

#[cfg(test)]
mod tests;
