//! Ordered broker shutdown and blocking reap.
//!
//! The executor asks the broker to perform child cleanup from inside Tokio, then
//! drops the runtime and reaps the broker synchronously. A timeout escalates to
//! killing only the known broker PID and waiting for that exact child event.
//!
//! After an abnormal broker exit the broker's surviving children reparent to
//! this supervisor, which holds the child-subreaper role, so the reap also
//! drains that orphaned subtree. Without it a workload the out-of-group group
//! guard kills would leak: on FreeBSD init never reaps a process orphaned from
//! an already-exited reaper. Draining is scoped to the broker's own
//! descendants, since nothing else is ever this process's child.

use std::time::{Duration, Instant};

use tokio::time::timeout;

use super::{
    BROKER_EVENT_TIMEOUT, BROKER_REAP_TIMEOUT, ChildEvent, ExecutorError, ProcessBrokerClient,
    ProcessBrokerEvent, ProcessSignal, SignalTarget, reap_any_event, signal, wait_for_event,
};

pub(super) async fn shutdown_broker(client: &mut ProcessBrokerClient) -> Result<(), ExecutorError> {
    client.shutdown().await?;
    loop {
        match timeout(BROKER_EVENT_TIMEOUT, client.next_event()).await {
            Ok(Ok(ProcessBrokerEvent::ShutdownComplete)) => return Ok(()),
            Ok(Ok(
                ProcessBrokerEvent::Child { .. }
                | ProcessBrokerEvent::TaskStarted { .. }
                | ProcessBrokerEvent::TaskSpawnFailed { .. }
                | ProcessBrokerEvent::TaskChild { .. }
                | ProcessBrokerEvent::TaskSignalDelivered { .. }
                | ProcessBrokerEvent::TaskSignalFailed { .. },
            )) => {}
            Ok(Ok(ProcessBrokerEvent::ShutdownFailed { .. })) => {
                return Err(ExecutorError::BrokerTimedOut("shutdown cleanup"));
            }
            Ok(Ok(event)) => return Err(ExecutorError::UnexpectedBrokerEvent(event)),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(ExecutorError::BrokerTimedOut("shutdown")),
        }
    }
}

pub(super) fn reap_broker(process: crate::process::ProcessId) -> Result<(), ExecutorError> {
    let deadline = Instant::now() + BROKER_REAP_TIMEOUT;
    let broker_outcome = loop {
        match reap_any_event() {
            Ok(Some(event)) if event.pid() == process && event.is_terminal() => {
                break match event {
                    ChildEvent::Exited { code: 0, .. } => Ok(()),
                    _ => Err(ExecutorError::BrokerExited(event)),
                };
            }
            // A reparented subtree orphan reaped ahead of the broker after an
            // abnormal exit, or an idle poll; keep waiting for the broker itself.
            Ok(Some(_) | None) => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            let _ = signal(SignalTarget::Process(process), ProcessSignal::Kill);
            let event = wait_for_event(process)?;
            return Err(ExecutorError::BrokerExited(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    drain_reparented_subtree(deadline);
    broker_outcome
}

/// Reap subtree orphans reparented after an abnormal broker exit.
///
/// The supervisor holds the child-subreaper role, so a crashed broker's killed
/// workloads reparent here as zombies that on FreeBSD init would never collect.
/// Reaping continues until `reap_any_event` reports `ECHILD` — the whole subtree
/// is reaped — or the shared deadline elapses. A still-running escaped
/// descendant that left its owned group only yields idle polls and is left to
/// its next reaper rather than blocking teardown indefinitely.
fn drain_reparented_subtree(deadline: Instant) {
    while Instant::now() < deadline {
        match reap_any_event() {
            Ok(Some(_)) => {}
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => return,
        }
    }
}
