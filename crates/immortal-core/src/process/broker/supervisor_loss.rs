//! Descriptor-tracked generation cleanup after the supervisor connection is lost.
//!
//! [`cleanup_after_supervisor_loss`] kills every owned group, reaps without
//! forwarding events (there is no supervisor left to receive them), and then
//! — if exactly one descriptor-tracked generation remains and a
//! [`BrokerLifetimePlan`] was configured — runs the fallback stop command and
//! waits for the tracked generation's lifetime descriptor to close. The
//! broker treats any survivor, timeout, or an ambiguous multi-generation
//! state as a hard failure rather than guessing at owner intent.

use std::collections::BTreeMap;
use std::io;

use tokio::signal::unix::Signal as ChildSignal;
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout_at};

use crate::supervisor::Generation;

use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::reap::{collect_child_events, has_workload_processes, signal_every_group};
use super::runtime::record_lifetime_observation;
use super::shutdown::{SHUTDOWN_GRACE, SHUTDOWN_KILL_WAIT};
use super::state::{BrokerGeneration, BrokerOwnedProcess, LifetimeObservation, LifetimeState};
use super::types::BrokerLifetimePlan;
use super::{ProcessId, ProcessSignal, SignalTarget, signal, spawn_process};

/// A descriptor-tracked generation paired with the plan for cleaning it up
/// after the supervisor connection is lost.
struct SupervisorLossCleanup {
    generation: Generation,
    plan: BrokerLifetimePlan,
}

pub(super) async fn cleanup_after_supervisor_loss(
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
    subreaper_active: bool,
) -> Result<(), ProcessBrokerError> {
    signal_every_group(generations, ProcessSignal::Kill);
    if !reap_without_events(
        Instant::now() + SHUTDOWN_KILL_WAIT,
        child_signal,
        generations,
        processes,
        logging,
        subreaper_active,
    )
    .await?
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "broker children survived supervisor-loss cleanup",
        )
        .into());
    }

    let mut tracked = generations.iter().filter_map(|(generation, entry)| {
        (entry.lifetime != LifetimeState::Foreground).then_some(*generation)
    });
    let generation = tracked.next();
    if tracked.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker owns more than one descriptor-tracked generation",
        )
        .into());
    }
    let complete = match (generation, lifetime_cleanup) {
        (Some(generation), Some(plan)) => {
            run_supervisor_loss_hook(
                SupervisorLossCleanup { generation, plan },
                child_signal,
                generations,
                processes,
                logging,
                lifetime_events,
                subreaper_active,
            )
            .await?
        }
        (None, _) => true,
        (Some(_), None) => false,
    };
    if complete {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "descriptor cleanup did not establish lifetime closure",
        )
        .into())
    }
}

async fn run_supervisor_loss_hook(
    cleanup: SupervisorLossCleanup,
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    subreaper_active: bool,
) -> Result<bool, ProcessBrokerError> {
    let SupervisorLossCleanup { generation, plan } = cleanup;
    let BrokerLifetimePlan {
        stop,
        stop_timeout,
        lifetime_timeout,
    } = plan;
    let hook = match spawn_process(stop, SHUTDOWN_GRACE) {
        Ok(hook) => {
            processes.insert(hook.process(), BrokerOwnedProcess::Generation(generation));
            Some(hook)
        }
        Err(error) => {
            if let Some(process) = error.cleanup_pending() {
                processes.insert(process, BrokerOwnedProcess::Generation(generation));
            }
            None
        }
    };
    let hook_finished = reap_without_events(
        Instant::now() + stop_timeout,
        child_signal,
        generations,
        processes,
        logging,
        subreaper_active,
    )
    .await?;
    if let Some(hook) = hook
        && !hook_finished
    {
        let _ = signal(SignalTarget::Group(hook.group()), ProcessSignal::Kill);
        let reaped = reap_without_events(
            Instant::now() + SHUTDOWN_KILL_WAIT,
            child_signal,
            generations,
            processes,
            logging,
            subreaper_active,
        )
        .await?;
        if !reaped {
            return Ok(false);
        }
    }
    let lifetime_closed = wait_for_cleanup_lifetime(
        generation,
        Instant::now() + lifetime_timeout,
        generations,
        processes,
        lifetime_events,
    )
    .await?;
    Ok(hook_finished && lifetime_closed)
}

async fn reap_without_events(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    subreaper_active: bool,
) -> Result<bool, ProcessBrokerError> {
    let _ = collect_child_events(generations, processes, logging, subreaper_active)?;
    while has_workload_processes(processes) {
        match timeout_at(deadline, child_signal.recv()).await {
            Ok(Some(())) => {
                let _ = collect_child_events(generations, processes, logging, subreaper_active)?;
            }
            Ok(None) => {
                return Err(ProcessBrokerError(
                    ProcessBrokerErrorKind::SignalStreamClosed,
                ));
            }
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

async fn wait_for_cleanup_lifetime(
    generation: Generation,
    deadline: Instant,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
) -> Result<bool, ProcessBrokerError> {
    if !generations.contains_key(&generation) {
        return Ok(true);
    }
    loop {
        let observation = match timeout_at(deadline, lifetime_events.recv()).await {
            Ok(Some(observation)) => observation,
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "broker lifetime event stream closed",
                )
                .into());
            }
            Err(_) => return Ok(false),
        };
        let observed_generation = observation.generation;
        let closed = observation.result.is_ok();
        let _ = record_lifetime_observation(&observation, generations, processes)?;
        if observed_generation == generation {
            return Ok(closed && !generations.contains_key(&generation));
        }
    }
}
