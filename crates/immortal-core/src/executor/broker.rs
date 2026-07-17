//! Ordered broker shutdown and blocking reap.
//!
//! The executor asks the broker to perform child cleanup from inside Tokio, then
//! drops the runtime and reaps the broker synchronously. A timeout escalates to
//! killing only the known broker PID and waits for that exact child event, so no
//! unrelated process can be signaled or consumed.

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
    loop {
        match reap_any_event() {
            Ok(Some(event)) if event.pid() == process && event.is_terminal() => {
                return match event {
                    ChildEvent::Exited { code: 0, .. } => Ok(()),
                    _ => Err(ExecutorError::BrokerExited(event)),
                };
            }
            Ok(Some(event)) => return Err(ExecutorError::UnexpectedChildEvent(event)),
            Ok(None) => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            let _ = signal(SignalTarget::Process(process), ProcessSignal::Kill);
            let event = wait_for_event(process)?;
            return Err(ExecutorError::BrokerExited(event));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
