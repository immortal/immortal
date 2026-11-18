//! Broker-backed checked launches of `immortal` supervisors.
//!
//! A launcher connects to one process broker that was created before Tokio,
//! prepares all commands before submission, and then waits for bounded readiness,
//! start, and terminal events. Batch state is task-local: individual spawn or
//! nonzero-exit outcomes are returned per launch, while broker protocol,
//! ordering, and task-space failures abort the whole batch.
//!
//! A batch has no deadline of its own. Each launch is bounded by itself: the
//! broker bounds the launcher's start and the launcher bounds its daemon's
//! startup handshake. A batch that is still waiting is therefore only slow,
//! because the host is starved or paused, and is reported once a minute rather
//! than abandoned; abandoning it ended the whole directory manager, although
//! every launch was still on its way. Job control of a launcher changes nothing.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};

use super::limits::LaunchConcurrency;
use crate::{
    config::{ConfigError, ServiceConfig},
    process::{
        BrokerTaskId, ChildEvent, ProcessBrokerEndpoint, ProcessBrokerEvent, ProcessCommand,
        wait_for_event,
    },
};

const SUPERVISOR_START_TIMEOUT: Duration = Duration::from_secs(15);

/// How often a launch batch that is still waiting is reported.
pub const LAUNCH_REPORT_INTERVAL: Duration = Duration::from_mins(1);
/// Failure while launching a checked supervisor through the pre-Tokio broker.
#[derive(Debug)]
pub enum LauncherError {
    /// Launcher command configuration was invalid.
    Config(ConfigError),
    /// Broker transport or process preparation failed.
    Broker(crate::process::ProcessBrokerError),
    /// Supervisor launcher could not be prepared or the broker could not be reaped.
    OperatingSystem(io::Error),
    /// Launcher did not complete within its hard deadline.
    Timeout,
    /// A caller submitted more launches than its validated batch limit.
    BatchLimit { actual: usize, limit: usize },
    /// Broker returned an event unrelated to the current launch task.
    UnexpectedEvent(ProcessBrokerEvent),
    /// The checked `immortal` launcher exited unsuccessfully.
    LauncherExited(ChildEvent),
}

impl Display for LauncherError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => Display::fmt(error, formatter),
            Self::Broker(error) => Display::fmt(error, formatter),
            Self::OperatingSystem(error) => Display::fmt(error, formatter),
            Self::Timeout => formatter.write_str("supervisor launcher deadline exceeded"),
            Self::BatchLimit { actual, limit } => {
                write!(
                    formatter,
                    "launch batch size {actual} exceeds limit {limit}"
                )
            }
            Self::UnexpectedEvent(event) => {
                write!(formatter, "unexpected supervisor launcher event: {event:?}")
            }
            Self::LauncherExited(event) => {
                write!(
                    formatter,
                    "supervisor launcher exited unsuccessfully: {event:?}"
                )
            }
        }
    }
}

impl Error for LauncherError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Broker(error) => Some(error),
            Self::OperatingSystem(error) => Some(error),
            Self::Timeout
            | Self::BatchLimit { .. }
            | Self::UnexpectedEvent(_)
            | Self::LauncherExited(_) => None,
        }
    }
}

impl From<ConfigError> for LauncherError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<crate::process::ProcessBrokerError> for LauncherError {
    fn from(error: crate::process::ProcessBrokerError) -> Self {
        Self::Broker(error)
    }
}

impl From<io::Error> for LauncherError {
    fn from(error: io::Error) -> Self {
        Self::OperatingSystem(error)
    }
}

/// Checked-daemon launcher backed by one broker created before Tokio.
pub struct SupervisorLauncher {
    client: crate::process::ProcessBrokerClient,
    next_task: u64,
}

/// Owned paths for one checked supervisor launch.
#[derive(Debug, Eq, PartialEq)]
pub struct SupervisorLaunch {
    snapshot: PathBuf,
    runtime_directory: PathBuf,
}

impl SupervisorLaunch {
    /// Construct one launch from a normalized snapshot and exact runtime path.
    #[must_use]
    pub fn new(snapshot: PathBuf, runtime_directory: PathBuf) -> Self {
        Self {
            snapshot,
            runtime_directory,
        }
    }
}

/// Isolated result for one submitted launcher task.
#[derive(Debug)]
pub enum LauncherTaskError {
    /// The broker could not execute the launcher command.
    Spawn,
    /// The launcher invoked `immortal` but it exited unsuccessfully.
    Exited(ChildEvent),
}

impl Display for LauncherTaskError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Spawn => formatter.write_str("broker could not execute the immortal launcher"),
            Self::Exited(event) => write!(formatter, "supervisor launcher failed: {event:?}"),
        }
    }
}

impl Error for LauncherTaskError {}

impl From<LauncherTaskError> for LauncherError {
    fn from(error: LauncherTaskError) -> Self {
        match error {
            LauncherTaskError::Spawn => {
                Self::OperatingSystem(io::Error::other("broker could not execute launcher"))
            }
            LauncherTaskError::Exited(event) => Self::LauncherExited(event),
        }
    }
}

#[derive(Debug, Default)]
struct LaunchTaskState {
    started: bool,
    outcome: Option<Result<(), LauncherTaskError>>,
}

impl SupervisorLauncher {
    /// Connect an endpoint and require the broker's bounded readiness event.
    ///
    /// # Errors
    ///
    /// Returns a broker, timeout, or unexpected-event failure.
    pub async fn connect(endpoint: ProcessBrokerEndpoint) -> Result<Self, LauncherError> {
        let mut client = endpoint.connect()?;
        match timeout(SUPERVISOR_START_TIMEOUT, client.next_event()).await {
            Ok(Ok(ProcessBrokerEvent::Ready)) => Ok(Self {
                client,
                next_task: 1,
            }),
            Ok(Ok(event)) => Err(LauncherError::UnexpectedEvent(event)),
            Ok(Err(error)) => Err(error.into()),
            Err(_) => Err(LauncherError::Timeout),
        }
    }

    /// Submit one bounded batch and return task outcomes in input order.
    ///
    /// Commands and task IDs are fully prepared before the first broker write.
    /// A task-local spawn/nonzero-exit failure does not discard other outcomes;
    /// broker or protocol failure aborts the complete batch. While launches are
    /// still outstanding, `report` receives their number and the time waited
    /// so far once every [`LAUNCH_REPORT_INTERVAL`].
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized batch, invalid command preparation,
    /// task-space exhaustion, or broker transport/protocol failure. An empty
    /// batch succeeds with an empty result.
    pub async fn launch_batch<R>(
        &mut self,
        binary: &Path,
        launches: Vec<SupervisorLaunch>,
        concurrency: LaunchConcurrency,
        mut report: R,
    ) -> Result<Vec<Result<(), LauncherTaskError>>, LauncherError>
    where
        R: FnMut(usize, Duration),
    {
        if launches.len() > concurrency.get() {
            return Err(LauncherError::BatchLimit {
                actual: launches.len(),
                limit: concurrency.get(),
            });
        }
        let mut prepared = Vec::with_capacity(launches.len());
        let mut order = Vec::with_capacity(launches.len());
        let mut states = BTreeMap::new();
        for launch in launches {
            let task = self.allocate_task()?;
            let command =
                prepare_launcher_command(binary, &launch.snapshot, &launch.runtime_directory)?;
            order.push(task);
            states.insert(task, LaunchTaskState::default());
            prepared.push((task, command));
        }
        for (task, command) in prepared {
            self.client
                .spawn_task(task, command, SUPERVISOR_START_TIMEOUT)
                .await?;
        }
        if states.is_empty() {
            return Ok(Vec::new());
        }
        self.wait_for_launch_batch(&mut states, LAUNCH_REPORT_INTERVAL, &mut report)
            .await?;
        order
            .into_iter()
            .map(|task| {
                states
                    .remove(&task)
                    .and_then(|state| state.outcome)
                    .ok_or_else(|| {
                        LauncherError::OperatingSystem(io::Error::other(
                            "completed launch task has no outcome",
                        ))
                    })
            })
            .collect()
    }

    fn allocate_task(&mut self) -> Result<BrokerTaskId, LauncherError> {
        let task = BrokerTaskId::new(self.next_task)
            .ok_or_else(|| io::Error::other("supervisor launcher task space exhausted"))?;
        self.next_task = self
            .next_task
            .checked_add(1)
            .ok_or_else(|| io::Error::other("supervisor launcher task counter overflow"))?;
        Ok(task)
    }

    /// Wait until every task of the batch has an outcome, reporting the wait
    /// every `interval`.
    ///
    /// All progress lives in `states`, so the wait itself holds nothing a
    /// report could lose.
    async fn wait_for_launch_batch<R>(
        &mut self,
        states: &mut BTreeMap<BrokerTaskId, LaunchTaskState>,
        interval: Duration,
        report: &mut R,
    ) -> Result<(), LauncherError>
    where
        R: FnMut(usize, Duration),
    {
        // A zero period would be meaningless and is refused by the timer.
        let interval = interval.max(Duration::from_millis(1));
        let started = Instant::now();
        let mut reports = interval_at(started + interval, interval);
        reports.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            let remaining = states
                .values()
                .filter(|state| state.outcome.is_none())
                .count();
            if remaining == 0 {
                return Ok(());
            }
            tokio::select! {
                biased;
                event = self.client.next_event() => record_launch_event(states, event?)?,
                _ = reports.tick() => report(remaining, started.elapsed()),
            }
        }
    }

    /// Stop and reap the otherwise childless launch broker.
    ///
    /// # Errors
    ///
    /// Returns a broker, timeout, unexpected-event, or broker-exit failure.
    pub async fn shutdown(mut self) -> Result<(), LauncherError> {
        let process = self.client.process();
        self.client.shutdown().await?;
        match timeout(SUPERVISOR_START_TIMEOUT, self.client.next_event()).await {
            Ok(Ok(ProcessBrokerEvent::ShutdownComplete)) => {}
            Ok(Ok(event)) => return Err(LauncherError::UnexpectedEvent(event)),
            Ok(Err(error)) => return Err(error.into()),
            Err(_) => return Err(LauncherError::Timeout),
        }
        drop(self.client);
        let event = wait_for_event(process)?;
        if matches!(event, ChildEvent::Exited { code: 0, .. }) {
            Ok(())
        } else {
            Err(LauncherError::LauncherExited(event))
        }
    }
}

/// Apply one broker event to the batch it must belong to.
///
/// # Errors
///
/// Returns an error for an event of an unknown task or one out of order.
fn record_launch_event(
    states: &mut BTreeMap<BrokerTaskId, LaunchTaskState>,
    event: ProcessBrokerEvent,
) -> Result<(), LauncherError> {
    match &event {
        ProcessBrokerEvent::TaskStarted { task, .. } => {
            let Some(state) = states.get_mut(task) else {
                return Err(LauncherError::UnexpectedEvent(event));
            };
            if state.started || state.outcome.is_some() {
                return Err(LauncherError::OperatingSystem(io::Error::other(
                    "duplicate or late launcher start event",
                )));
            }
            state.started = true;
        }
        ProcessBrokerEvent::TaskChild {
            task,
            event: child_event,
        } if child_event.is_terminal() => {
            let Some(state) = states.get_mut(task) else {
                return Err(LauncherError::UnexpectedEvent(event));
            };
            if !state.started || state.outcome.is_some() {
                return Err(LauncherError::OperatingSystem(io::Error::other(
                    "launcher terminal event violated task ordering",
                )));
            }
            state.outcome = Some(match child_event {
                ChildEvent::Exited { code: 0, .. } => Ok(()),
                terminal => Err(LauncherTaskError::Exited(*terminal)),
            });
        }
        // A stop or continue of a launcher's own job control changes nothing:
        // the launch is still on its way and ends with its terminal event.
        ProcessBrokerEvent::TaskChild { task, .. } => {
            let Some(state) = states.get(task) else {
                return Err(LauncherError::UnexpectedEvent(event));
            };
            if !state.started || state.outcome.is_some() {
                return Err(LauncherError::OperatingSystem(io::Error::other(
                    "launcher job-control event violated task ordering",
                )));
            }
        }
        ProcessBrokerEvent::TaskSpawnFailed { task, .. } => {
            let Some(state) = states.get_mut(task) else {
                return Err(LauncherError::UnexpectedEvent(event));
            };
            if state.started || state.outcome.is_some() {
                return Err(LauncherError::OperatingSystem(io::Error::other(
                    "launcher spawn failure violated task ordering",
                )));
            }
            state.outcome = Some(Err(LauncherTaskError::Spawn));
        }
        _ => return Err(LauncherError::UnexpectedEvent(event)),
    }
    Ok(())
}

fn prepare_launcher_command(
    binary: &Path,
    snapshot: &Path,
    runtime_directory: &Path,
) -> Result<ProcessCommand, LauncherError> {
    let binary_text = binary.to_str().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "immortal binary path is not UTF-8",
        )
    })?;
    let config = ServiceConfig::for_command(vec![binary_text.to_owned()])?;
    let mut command = ProcessCommand::from_service(&config, std::env::vars_os())?;
    command
        .argument("--config")
        .argument(snapshot.as_os_str())
        .argument("--control-dir")
        .argument(runtime_directory.as_os_str());
    Ok(command)
}

#[cfg(test)]
mod tests;
