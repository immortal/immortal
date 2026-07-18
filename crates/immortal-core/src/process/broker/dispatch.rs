//! Request-to-handler routing for the broker's single connected supervisor.
//!
//! [`handle_broker_request`] is the sole entry point the event loop calls; it
//! owns no state itself and forwards each decoded request to its focused
//! handler. `Detach` and `Signal` handling stay local to this module because
//! neither is reused elsewhere: detach only succeeds when the broker owns
//! exactly the one foreground generation being released, and signal
//! resolves the requested scope against the generation's live child or group.

use std::collections::BTreeMap;
use std::io;

use tokio::io::AsyncWrite;
use tokio::signal::unix::Signal as ChildSignal;

use crate::supervisor::Generation;

use super::error::ProcessBrokerError;
use super::reap::disarm_generation_guard;
use super::shutdown::shutdown_owned;
use super::spawn::{handle_logger_spawn, handle_spawn};
use super::state::{
    BrokerGeneration, BrokerOwnedProcess, BrokerRuntimeState, BrokerSpawnState, LifetimeState,
};
use super::wire::write_event;
use super::{
    BrokerEvent, BrokerRequest, BrokerSignalTarget, ProcessId, ProcessSignal, SignalTarget, signal,
};

pub(super) async fn handle_broker_request<W>(
    request: BrokerRequest,
    child_signal: &mut ChildSignal,
    writer: &mut W,
    state: &mut BrokerRuntimeState,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    match request {
        BrokerRequest::Spawn {
            generation,
            command,
            startup_timeout,
            readiness_timeout,
            lifetime_tracking,
        } => {
            handle_spawn(
                generation,
                command,
                startup_timeout,
                readiness_timeout,
                lifetime_tracking,
                writer,
                BrokerSpawnState {
                    generations: &mut state.generations,
                    lifetime_sender: &state.lifetime_sender,
                    processes: &mut state.processes,
                    readiness_sender: &state.readiness_sender,
                    logging: &state.logging,
                },
            )
            .await?;
        }
        BrokerRequest::SpawnLogger {
            generation,
            logger,
            startup_timeout,
        } => {
            handle_logger_spawn(
                generation,
                logger,
                startup_timeout,
                writer,
                &mut state.generations,
                &mut state.processes,
                &mut state.logging,
            )
            .await?;
        }
        BrokerRequest::CloseLoggerInputs => {
            state.logging.close_writer_masters();
            write_event(writer, &BrokerEvent::LoggerInputsClosed).await?;
        }
        BrokerRequest::Signal {
            generation,
            target,
            signal: requested,
        } => {
            handle_signal(generation, target, requested, writer, &state.generations).await?;
        }
        BrokerRequest::Detach { generation } => {
            handle_detach(
                generation,
                writer,
                &mut state.generations,
                &mut state.processes,
            )
            .await?;
        }
        BrokerRequest::Shutdown => {
            let complete = shutdown_owned(
                child_signal,
                writer,
                &mut state.generations,
                &mut state.processes,
                &mut state.logging,
                state.subreaper_active,
            )
            .await?;
            let event = if complete {
                BrokerEvent::ShutdownComplete
            } else {
                BrokerEvent::ShutdownFailed { os_error: None }
            };
            write_event(writer, &event).await?;
            return Ok(complete);
        }
    }
    Ok(false)
}

async fn handle_detach<W>(
    generation: Generation,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let Some(child) = generations
        .get(&generation)
        .filter(|entry| entry.lifetime == LifetimeState::Foreground)
        .and_then(|entry| entry.child)
    else {
        return write_event(writer, &BrokerEvent::DetachFailed { generation }).await;
    };
    let guard_process = generations
        .get(&generation)
        .and_then(|entry| entry.guard.as_ref())
        .and_then(|guard| guard.process().ok());
    if generations.len() != 1
        || processes.len() != 2
        || processes.get(&child.process()) != Some(&BrokerOwnedProcess::Generation(generation))
        || guard_process.is_none_or(|guard| {
            processes.get(&guard) != Some(&BrokerOwnedProcess::Guard(generation))
        })
    {
        return write_event(writer, &BrokerEvent::DetachFailed { generation }).await;
    }
    if disarm_generation_guard(generation, generations, processes).is_err() {
        let _ = signal(SignalTarget::Group(child.group()), ProcessSignal::Kill);
        return write_event(writer, &BrokerEvent::DetachFailed { generation }).await;
    }
    generations.remove(&generation);
    processes.remove(&child.process());
    write_event(writer, &BrokerEvent::Detached { generation }).await
}

async fn handle_signal<W>(
    generation: Generation,
    target: BrokerSignalTarget,
    requested: ProcessSignal,
    writer: &mut W,
    generations: &BTreeMap<Generation, BrokerGeneration>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let result = generations.get(&generation).map_or_else(
        || {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "generation is not owned",
            ))
        },
        |entry| {
            let child = entry.child.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "generation has no direct child")
            })?;
            let target = match target {
                BrokerSignalTarget::Process => SignalTarget::Process(child.process()),
                BrokerSignalTarget::Group => SignalTarget::Group(child.group()),
            };
            signal(target, requested)
        },
    );
    let event = match result {
        Ok(()) => BrokerEvent::SignalDelivered { generation },
        Err(error) => BrokerEvent::SignalFailed {
            generation,
            os_error: error.raw_os_error(),
        },
    };
    write_event(writer, &event).await
}
