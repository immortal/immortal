//! Broker event loop: request dispatch, `SIGCHLD` reap sweep, and readiness
//! or lifetime observation delivery.
//!
//! [`run_broker`] is a single `tokio::select!` over the supervisor request
//! stream, the `SIGCHLD` signal (immediate reap), a low-frequency reap
//! interval (closes platform notification gaps), and the readiness/lifetime
//! observation channels populated by detached wait tasks. Supervisor EOF
//! routes to supervisor-loss cleanup instead of returning an error, since
//! losing the connection is an expected, handled transition rather than a
//! broker fault.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::net::UnixStream;
use tokio::signal::unix::{SignalKind, signal as listen_for_signal};
use tokio::sync::mpsc;
use tokio::time::{Instant, MissedTickBehavior, interval_at};

use crate::readiness::ReadinessError;
use crate::supervisor::Generation;

use super::dispatch::handle_broker_request;
use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::reap::{disarm_generation_guard, forward_child_events};
use super::state::{
    BrokerGeneration, BrokerOwnedProcess, BrokerRuntimeState, LifetimeObservation, LifetimeState,
};
use super::supervisor_loss::cleanup_after_supervisor_loss;
use super::types::BrokerLifetimePlan;
use super::wire::{read_request, write_event};
use super::{BrokerEvent, BrokerReadinessFailure, ProcessId};

const CHILD_REAP_INTERVAL: Duration = Duration::from_millis(250);
const READINESS_EVENT_CAPACITY: usize = 32;
const LIFETIME_EVENT_CAPACITY: usize = 32;

pub(super) async fn run_broker(
    stream: UnixStream,
    logging: BrokerLogging,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
) -> Result<(), ProcessBrokerError> {
    let (mut reader, mut writer) = stream.into_split();
    let mut child_signal = listen_for_signal(SignalKind::child())?;
    let (readiness_sender, mut readiness_events) = mpsc::channel(READINESS_EVENT_CAPACITY);
    let (lifetime_sender, mut lifetime_events) = mpsc::channel(LIFETIME_EVENT_CAPACITY);
    let mut child_reap = interval_at(Instant::now() + CHILD_REAP_INTERVAL, CHILD_REAP_INTERVAL);
    child_reap.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut state = BrokerRuntimeState {
        generations: BTreeMap::new(),
        logging,
        lifetime_cleanup,
        lifetime_sender,
        processes: BTreeMap::new(),
        readiness_sender,
    };
    write_event(&mut writer, &BrokerEvent::Ready).await?;

    loop {
        tokio::select! {
            request = read_request(&mut reader) => {
                let request = match request {
                    Ok(request) => request,
                    Err(error) if error.is_end_of_stream() => {
                        cleanup_after_supervisor_loss(
                            &mut child_signal,
                            &mut state.generations,
                            &mut state.processes,
                            &mut state.logging,
                            &mut lifetime_events,
                            state.lifetime_cleanup.take(),
                        ).await?;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                if handle_broker_request(request, &mut child_signal, &mut writer, &mut state).await? {
                    return Ok(());
                }
            }
            signal = child_signal.recv() => {
                if signal.is_none() {
                    return Err(ProcessBrokerError(ProcessBrokerErrorKind::SignalStreamClosed));
                }
                forward_child_events(
                    &mut writer,
                    &mut state.generations,
                    &mut state.processes,
                    &mut state.logging,
                ).await?;
            }
            _ = child_reap.tick(), if !state.processes.is_empty() => {
                forward_child_events(
                    &mut writer,
                    &mut state.generations,
                    &mut state.processes,
                    &mut state.logging,
                ).await?;
            }
            Some(observation) = readiness_events.recv() => {
                if state.generations.contains_key(&observation.generation) {
                    let event = match observation.result {
                        Ok(()) => BrokerEvent::GenerationReady {
                            generation: observation.generation,
                        },
                        Err(error) => BrokerEvent::ReadinessFailed {
                            generation: observation.generation,
                            failure: readiness_failure(&error),
                        },
                    };
                    write_event(&mut writer, &event).await?;
                }
            }
            Some(observation) = lifetime_events.recv() => {
                handle_lifetime_observation(
                    observation,
                    &mut writer,
                    &mut state.generations,
                    &mut state.processes,
                ).await?;
            }
        }
    }
}

async fn handle_lifetime_observation<W>(
    observation: LifetimeObservation,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(event) = record_lifetime_observation(&observation, generations, processes)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

pub(super) fn record_lifetime_observation(
    observation: &LifetimeObservation,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<Option<BrokerEvent>, ProcessBrokerError> {
    let Some(entry) = generations.get_mut(&observation.generation) else {
        return Ok(None);
    };
    if entry.lifetime != LifetimeState::Tracking {
        return Ok(None);
    }
    let event = if observation.result.is_ok() {
        entry.lifetime = LifetimeState::Closed;
        BrokerEvent::LifetimeClosed {
            generation: observation.generation,
        }
    } else {
        entry.lifetime = LifetimeState::Failed;
        BrokerEvent::LifetimeFailed {
            generation: observation.generation,
        }
    };
    let generation_finished = entry.child.is_none();
    if generation_finished {
        disarm_generation_guard(observation.generation, generations, processes)?;
        generations.remove(&observation.generation);
    }
    Ok(Some(event))
}

fn readiness_failure(error: &ReadinessError) -> BrokerReadinessFailure {
    match error {
        ReadinessError::Timeout => BrokerReadinessFailure::Timeout,
        ReadinessError::Io(_) => BrokerReadinessFailure::Descriptor,
        ReadinessError::InvalidToken => BrokerReadinessFailure::InvalidToken,
    }
}
