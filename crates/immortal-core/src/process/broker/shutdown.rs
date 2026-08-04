//! Bounded terminate-then-kill shutdown of every broker-owned generation.
//!
//! [`shutdown_owned`] signals every live group with `SIGTERM`, waits up to
//! [`SHUTDOWN_GRACE`], then escalates to `SIGKILL` and waits up to
//! [`SHUTDOWN_KILL_WAIT`] before reporting whether every workload process was
//! reaped. Both deadlines are also reused by supervisor-loss cleanup so the
//! two termination paths share one bounded shape.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::AsyncWrite;
use tokio::signal::unix::Signal as ChildSignal;
use tokio::time::{Instant, timeout_at};

use crate::supervisor::Generation;

use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::reap::{forward_child_events, has_workload_processes, signal_every_group};
use super::state::{BrokerGeneration, BrokerOwnedProcess};
use super::{ProcessId, ProcessSignal};

pub(super) const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
pub(super) const SHUTDOWN_KILL_WAIT: Duration = Duration::from_secs(3);

/// Stop every owned generation in order, escalating only if it is required.
///
/// Signals each live process group with its configured stop signal, keeps
/// forwarding child events to the supervisor so the reaps are not lost, and
/// waits up to [`SHUTDOWN_GRACE`] for the workloads to exit. Anything still
/// running is then killed and reaped within [`SHUTDOWN_KILL_WAIT`]. Guards are
/// disarmed rather than killed, so a workload which exits normally during the
/// grace window is never escalated. Logging adapters are drained last so the
/// final output of a stopping service is not truncated.
///
/// # Errors
///
/// Returns an error when signalling, waiting, or forwarding fails in a way
/// which leaves ownership unresolved, which the caller treats as a broker
/// fault rather than a clean shutdown.
pub(super) async fn shutdown_owned<W>(
    child_signal: &mut ChildSignal,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    subreaper_active: bool,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    signal_every_group(generations, ProcessSignal::Terminate);
    if reap_until(
        Instant::now() + SHUTDOWN_GRACE,
        child_signal,
        writer,
        generations,
        processes,
        logging,
        subreaper_active,
    )
    .await?
    {
        return Ok(true);
    }
    signal_every_group(generations, ProcessSignal::Kill);
    reap_until(
        Instant::now() + SHUTDOWN_KILL_WAIT,
        child_signal,
        writer,
        generations,
        processes,
        logging,
        subreaper_active,
    )
    .await
}

async fn reap_until<W>(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    logging: &mut BrokerLogging,
    subreaper_active: bool,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    forward_child_events(writer, generations, processes, logging, subreaper_active).await?;
    while has_workload_processes(processes) {
        match timeout_at(deadline, child_signal.recv()).await {
            Ok(Some(())) => {
                forward_child_events(writer, generations, processes, logging, subreaper_active)
                    .await?;
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
