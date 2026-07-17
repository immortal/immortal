//! Broker-backed checked launches of `immortal` supervisors.
//!
//! A launcher connects to one process broker that was created before Tokio,
//! prepares all commands before submission, and then waits for bounded readiness,
//! start, and terminal events. Batch state is task-local: individual spawn or
//! nonzero-exit outcomes are returned per launch, while broker protocol,
//! ordering, task-space, and deadline failures abort the whole batch.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::{Path, PathBuf},
    time::Duration,
};

use tokio::time::timeout;

use super::limits::LaunchConcurrency;
use crate::{
    config::{ConfigError, ServiceConfig},
    process::{
        BrokerTaskId, ChildEvent, ProcessBrokerEndpoint, ProcessBrokerEvent, ProcessCommand,
        wait_for_event,
    },
};

const SUPERVISOR_START_TIMEOUT: Duration = Duration::from_secs(15);

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

    /// Launch one checked daemon and wait for its invoking process to report success.
    ///
    /// # Errors
    ///
    /// Returns preparation, broker, timeout, exec, or nonzero launcher-exit failure.
    pub async fn launch(
        &mut self,
        binary: &Path,
        snapshot: &Path,
        runtime_directory: &Path,
    ) -> Result<(), LauncherError> {
        let command = prepare_launcher_command(binary, snapshot, runtime_directory)?;
        let task = self.allocate_task()?;
        self.client
            .spawn_task(task, command, SUPERVISOR_START_TIMEOUT)
            .await?;
        timeout(SUPERVISOR_START_TIMEOUT, self.wait_for_launcher(task))
            .await
            .map_err(|_| LauncherError::Timeout)?
    }

    /// Submit one bounded batch and return task outcomes in input order.
    ///
    /// Commands and task IDs are fully prepared before the first broker write.
    /// A task-local spawn/nonzero-exit failure does not discard other outcomes;
    /// broker, protocol, or deadline failure aborts the complete batch.
    ///
    /// # Errors
    ///
    /// Returns an error for an oversized batch, invalid command preparation,
    /// task-space exhaustion, broker transport/protocol failure, or the shared
    /// batch deadline. An empty batch succeeds with an empty result.
    pub async fn launch_batch(
        &mut self,
        binary: &Path,
        launches: Vec<SupervisorLaunch>,
        concurrency: LaunchConcurrency,
    ) -> Result<Vec<Result<(), LauncherTaskError>>, LauncherError> {
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
        timeout(
            SUPERVISOR_START_TIMEOUT,
            self.wait_for_launch_batch(&mut states),
        )
        .await
        .map_err(|_| LauncherError::Timeout)??;
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

    async fn wait_for_launcher(&mut self, task: BrokerTaskId) -> Result<(), LauncherError> {
        loop {
            match self.client.next_event().await? {
                ProcessBrokerEvent::TaskStarted { task: current, .. } if current == task => {}
                ProcessBrokerEvent::TaskChild {
                    task: current,
                    event: ChildEvent::Exited { code: 0, .. },
                } if current == task => return Ok(()),
                ProcessBrokerEvent::TaskChild {
                    task: current,
                    event,
                } if current == task && event.is_terminal() => {
                    return Err(LauncherError::LauncherExited(event));
                }
                ProcessBrokerEvent::TaskSpawnFailed { task: current, .. } if current == task => {
                    return Err(LauncherError::OperatingSystem(io::Error::other(
                        "broker could not execute the immortal launcher",
                    )));
                }
                event => return Err(LauncherError::UnexpectedEvent(event)),
            }
        }
    }

    async fn wait_for_launch_batch(
        &mut self,
        states: &mut BTreeMap<BrokerTaskId, LaunchTaskState>,
    ) -> Result<(), LauncherError> {
        let mut remaining = states.len();
        while remaining != 0 {
            let event = self.client.next_event().await?;
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
                    remaining = remaining
                        .checked_sub(1)
                        .ok_or_else(|| io::Error::other("launcher completion counter underflow"))?;
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
                    remaining = remaining
                        .checked_sub(1)
                        .ok_or_else(|| io::Error::other("launcher completion counter underflow"))?;
                }
                _ => return Err(LauncherError::UnexpectedEvent(event)),
            }
        }
        Ok(())
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
