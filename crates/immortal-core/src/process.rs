//! Child process creation, daemonization, ownership, environment, and signals.
//!
//! This module is the crate's only boundary onto Unix process primitives.
//! Fork, session, and wait mechanics come from the `fork` crate; every
//! function here translates its checked contract into Immortal's own stable
//! types so process-group ownership, descriptor survival, and startup
//! handshakes never leak raw `libc` conventions into the supervisor,
//! executor, or CLI crates.
//!
//! `process.rs` stays a thin facade over cohesively split child modules:
//!
//! - `identity` — checked, always-positive process and process-group
//!   identifiers, and the explicit [`SignalTarget`] they compose into.
//! - `signal` — the portable [`ProcessSignal`] vocabulary and the single
//!   checked delivery entry point.
//! - `command` — deterministic command, environment, and numeric-identity
//!   preparation, performed before the broker forks so configuration mistakes
//!   surface as typed errors in the supervisor.
//! - `daemon` — the checked double-fork/session/handshake sequence that
//!   detaches the supervisor before Tokio starts.
//! - `spawn` — direct child creation: process-group reservation, descriptor
//!   allow-listing, and the child startup handshake.
//! - `wait` — blocking and non-blocking drains of the `fork` wait boundary
//!   into Immortal's [`ChildEvent`].
//! - `subreaper` — registers the broker as the reaper for orphaned descendants
//!   so escaped grandchildren reparent to it and drain through `wait` instead
//!   of leaking to init.
//! - `broker` and `broker_protocol` — the dedicated single-threaded process
//!   broker and the bounded wire contract it speaks with the supervisor.
//!
//! Every child module is private; this file re-exports the deliberate public
//! and crate-internal surface so `immortal_core::process::*` keeps one
//! canonical path per type regardless of which file implements it.

mod broker;
mod broker_protocol;
mod command;
mod daemon;
mod identity;
mod signal;
mod spawn;
mod subreaper;
mod wait;

pub(crate) use self::broker::{
    BrokerFileRoute, BrokerLoggerId, BrokerLoggingPlan, start_process_broker_with_logging,
};
pub use self::broker::{
    BrokerLifetimePlan, BrokerSignalScope, BrokerTaskId, ProcessBrokerClient,
    ProcessBrokerEndpoint, ProcessBrokerError, ProcessBrokerEvent, ReadinessFailure,
    start_process_broker, start_process_broker_with_lifetime,
};
pub use self::command::{
    ProcessCommand, ProcessCredentials, ProcessEnvironment, SupplementaryGroups,
    resolve_environment,
};
pub use self::daemon::{
    DaemonCleanup, DaemonError, DaemonFailure, DaemonStage, DaemonStartup, Daemonized, daemonize,
};
pub use self::identity::{
    InvalidProcessGroupId, InvalidProcessId, ProcessGroupId, ProcessId, SignalTarget,
};
pub use self::signal::{ProcessSignal, signal};
pub(crate) use self::spawn::ProcessGroupGuard;
// `spawn_with_descriptors_in_group` stays `pub(super)` in `spawn`, so this
// import keeps its original module-private reach: visible only within
// `process` and its descendants (chiefly `broker`), never crate-wide.
use self::spawn::spawn_with_descriptors_in_group;
pub use self::spawn::{
    ProcessDescriptor, SpawnError, SpawnFailure, SpawnStage, SpawnedProcess, spawn,
    spawn_with_descriptors,
};
pub use self::subreaper::{SubreaperStatus, acquire_subreaper, is_subreaper, release_subreaper};
pub use self::wait::{ChildEvent, reap_any_event, wait_for_event};
