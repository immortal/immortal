//! Dedicated single-threaded child-process broker and supervisor endpoint.
//!
//! The broker exclusively owns direct children, process groups, descriptor
//! endpoints, waits, and supervisor-loss cleanup. Descriptor generations retain
//! logical ownership after their launcher exits; EOF and the pre-runtime stop
//! plan are handled without adopting an application PID. `SIGCHLD` drives
//! immediate reaping, while a low-frequency sweep closes platform notification
//! gaps through the same ownership-checked wait path.
//!
//! Each spawn first reserves a process group with a short-lived anchor, joins
//! the workload, and then activates one out-of-group lifetime helper. The broker
//! owns the only disarm endpoint. Forced broker death closes it and kills the
//! group; deliberate cleanup disarms and reaps the helper. A helper event is a
//! containment failure, not an ordinary workload exit, and terminates policy
//! execution after the broker kills the affected group.
//!
//! Logging plans are also materialized before Tokio starts. The broker retains
//! stable service-route masters and one optional shared logger pipe, duplicating
//! only the descriptors each file adapter, service stream, or external logger
//! needs. It never copies bytes. Closing retained writers begins ordered EOF
//! drain, while child-held writer clones keep downstream input alive until every
//! upstream adapter exits.
//!
//! This facade only declares the child modules that own one cohesive part of
//! the broker and re-exports the exact surface this crate already depended on
//! before the split:
//!
//! - `logging` owns the pre-runtime logging graph and its stable pipes.
//! - `types` owns broker-local identity, signal scope, and the lifetime
//!   cleanup contract.
//! - `event`/`error` own the supervisor-facing event and error types.
//! - `client` owns the pre-runtime endpoint and the Tokio-side client.
//! - `launch` owns the fork entry points.
//! - `runtime` owns the event loop.
//! - `state` owns the broker-local lifecycle bookkeeping shared by the event
//!   loop, dispatch, spawn, and reap boundaries.
//! - `dispatch` routes one decoded request to its handler.
//! - `spawn` owns service/logger spawn preparation and group containment.
//! - `reap` owns child reaping, guard disarming, and group signaling.
//! - `shutdown` owns bounded terminate-then-kill shutdown.
//! - `supervisor_loss` owns descriptor-tracked cleanup after supervisor loss.
//! - `wire` owns the bounded frame read/write pairing used by every request
//!   and event.
//!
//! No behavior, wire byte, ownership rule, or error precedence changed when
//! this module was split; only the ownership boundaries between files did.

mod client;
mod dispatch;
mod error;
mod event;
mod launch;
mod logging;
mod reap;
mod runtime;
mod shutdown;
mod spawn;
mod state;
mod supervisor_loss;
mod types;
mod wire;

use super::{
    ChildEvent, ProcessCommand, ProcessDescriptor, ProcessGroupGuard, ProcessGroupId, ProcessId,
    ProcessSignal, SignalTarget, SpawnError, SpawnFailure, SpawnStage, SpawnedProcess,
    broker_protocol::{
        BrokerEvent, BrokerProtocolError, BrokerReadinessFailure, BrokerRequest,
        BrokerSignalTarget, HEADER_BYTES, MAX_FRAME_BYTES, declared_frame_length,
    },
    reap_any_event, signal, spawn as spawn_process, spawn_with_descriptors_in_group,
};

pub use self::client::{ProcessBrokerClient, ProcessBrokerEndpoint};
pub use self::error::ProcessBrokerError;
pub use self::event::ProcessBrokerEvent;
pub(crate) use self::launch::start_process_broker_with_logging;
pub use self::launch::{start_process_broker, start_process_broker_with_lifetime};
pub(crate) use self::logging::{BrokerFileRoute, BrokerLoggerId, BrokerLoggingPlan};
pub use self::types::{BrokerLifetimePlan, BrokerSignalScope, BrokerTaskId, ReadinessFailure};
