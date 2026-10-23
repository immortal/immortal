//! Request-to-handler routing for the broker's single connected supervisor.
//!
//! [`handle_broker_request`] is the sole entry point the event loop calls; it
//! owns no state itself and forwards each decoded request to its focused
//! handler. `Detach` and `Signal` handling stay local to this module because
//! neither is reused elsewhere: detach only succeeds when the broker owns
//! exactly the one foreground generation being released, and signal
//! resolves the requested scope against the generation's live child or group.
//!
//! A detached generation's group also leaves the retiring set, since the
//! broker no longer owns anything in it and must not await it at shutdown.

use std::collections::BTreeMap;
use std::io;

use tokio::io::AsyncWrite;
use tokio::signal::unix::Signal as ChildSignal;
use tokio::sync::mpsc;

use crate::supervisor::Generation;

use super::error::ProcessBrokerError;
use super::reap::{
    GroupPin, disarm_generation_guard, group_pin, kill_live_group, pinned_by_another,
    signal_live_group,
};
use super::retiring::RetiringGroups;
use super::shutdown::{ShutdownOutcome, shutdown_owned};
use super::spawn::{handle_logger_spawn, handle_spawn};
use super::state::{
    BrokerGeneration, BrokerOwnedProcess, BrokerRuntimeState, BrokerSpawnState,
    LifetimeObservation, LifetimeState,
};
use super::wire::write_event;
use super::{BrokerEvent, BrokerRequest, BrokerSignalTarget, ProcessSignal, SignalTarget, signal};

pub(super) async fn handle_broker_request<W>(
    request: BrokerRequest,
    child_signal: &mut ChildSignal,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
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
                    retiring: &mut state.retiring,
                },
            )
            .await?;
        }
        BrokerRequest::SpawnLogger {
            generation,
            logger,
            startup_timeout,
        } => {
            handle_logger_spawn(generation, logger, startup_timeout, writer, state).await?;
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
            handle_signal(
                generation,
                target,
                requested,
                writer,
                &state.generations,
                &mut state.retiring,
            )
            .await?;
        }
        BrokerRequest::Detach { generation } => {
            handle_detach(generation, writer, state).await?;
        }
        BrokerRequest::Shutdown => {
            let (event, complete) =
                match shutdown_owned(child_signal, lifetime_events, writer, state).await? {
                    ShutdownOutcome::Complete => (BrokerEvent::ShutdownComplete, true),
                    ShutdownOutcome::Failed { os_error } => {
                        (BrokerEvent::ShutdownFailed { os_error }, false)
                    }
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
    state: &mut BrokerRuntimeState,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let BrokerRuntimeState {
        generations,
        processes,
        retiring,
        ..
    } = state;
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
        // Disarming took the guard, so its anchor no longer pins the group and
        // the main-child reap will not kill or retire it; kill and retire it
        // here only if its unreaped PID equals the group ID.
        let pin = GroupPin {
            armed: false,
            reused: pinned_by_another(generations, generation, child.group()),
        };
        let _ = kill_live_group(child.process(), child.group(), pin, None, retiring, None);
        return write_event(writer, &BrokerEvent::DetachFailed { generation }).await;
    }
    generations.remove(&generation);
    processes.remove(&child.process());
    // The released group is no longer the broker's to drain.
    retiring.forget(child.group());
    write_event(writer, &BrokerEvent::Detached { generation }).await
}

/// Deliver a supervisor-requested signal to a generation's child or group.
///
/// A group request reaches the whole group while the generation's armed guard
/// or its unreaped, still-member direct child with PID == PGID pins the ID, and
/// also reaches a child which left the group; otherwise it reaches only that child. A group kill also retires the group, like every other kill, so
/// ordered shutdown awaits it even after the generation is gone.
async fn handle_signal<W>(
    generation: Generation,
    target: BrokerSignalTarget,
    requested: ProcessSignal,
    writer: &mut W,
    generations: &BTreeMap<Generation, BrokerGeneration>,
    retiring: &mut RetiringGroups,
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
            match target {
                BrokerSignalTarget::Process => {
                    signal(SignalTarget::Process(child.process()), requested)
                }
                BrokerSignalTarget::Group if requested == ProcessSignal::Kill => {
                    let pin = group_pin(generations, generation, entry);
                    kill_live_group(
                        child.process(),
                        child.group(),
                        pin,
                        entry.guard.as_ref(),
                        retiring,
                        None,
                    )
                }
                BrokerSignalTarget::Group => {
                    let pin = group_pin(generations, generation, entry);
                    signal_live_group(
                        child.process(),
                        child.group(),
                        requested,
                        pin,
                        entry.guard.as_ref(),
                        retiring,
                        None,
                    )
                }
            }
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
