//! Mutable supervision context and lifecycle state types.
//!
//! The executor loop owns all mutable policy state in one context: service
//! generation, restart tracking, deadline selection, logger phase, descriptor
//! lifetime, pending operator responses, PID-file publication, and status
//! snapshots. Child modules may mutate these fields only while handling one
//! serialized executor event, so no process or descriptor ownership crosses a
//! concurrent boundary.

use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

use tokio::time::Instant as TokioInstant;

use super::{
    BrokerLoggerId, BrokerSignalScope, BrokerTaskId, ChildResult, ConditionTracker, ControlCommand,
    DesiredState, FailureReason, Generation, LastResult, OverdueReports, OwnedPidFile,
    ProcessBrokerClient, ProcessBrokerError, ProcessBrokerEvent, ProcessId, ProcessSignal,
    Response, RestartTracker, ServiceConfig, StateMachine, StatusSnapshot, StopCompletion,
    SupervisorState, elapsed_seconds, logger_status,
};
use crate::status::{LoggerLastResult, LoggerStatus, SpawnDetail};

pub(super) enum ExecutorEvent {
    Broker(ProcessBrokerEvent),
    Shutdown,
    Control(ControlCommand),
    ControlClosed,
    Timer,
}

pub(super) enum PendingSignal {
    Lifecycle,
    Control {
        command: ControlCommand,
        success: Box<Response>,
    },
}

pub(super) struct ExecutionContext {
    pub(super) auxiliary: AuxiliaryExecution,
    pub(super) condition_tracker: ConditionTracker,
    pub(super) deadline: Option<TokioInstant>,
    pub(super) descriptor: Option<DescriptorExecution>,
    pub(super) epoch: Instant,
    pub(super) first_start: bool,
    /// Counts accepted operator requests that change what the supervisor
    /// wants, so a completion can tell whether one arrived while it was
    /// pending.
    pub(super) goal_revision: u64,
    /// Number of logger failures recorded so far, which orders them across
    /// loggers so status reports the most recent one.
    pub(super) logger_failures: u64,
    pub(super) task_signals: OwedTaskAcknowledgements,
    pub(super) logger_shutdown: LoggerShutdownState,
    pub(super) lifecycle: Option<LifecycleExecution>,
    pub(super) loggers: Vec<LoggerExecution>,
    pub(super) machine: StateMachine,
    pub(super) next_task: u64,
    /// Rate limit for reports of broker answers that are late.
    pub(super) overdue: OverdueReports,
    pub(super) pending_completion: Option<PendingCompletion>,
    pub(super) pending_detach: Option<PendingDetach>,
    pub(super) pending_signals: VecDeque<PendingSignal>,
    pub(super) pending_stop: Option<StopCompletion>,
    pub(super) status: RuntimeStatus,
    pub(super) stop_kill_sent: bool,
    pub(super) shutdown_requested: bool,
    pub(super) tracker: RestartTracker,
    pub(super) terminal_failure: Option<FailureReason>,
}

impl ExecutionContext {
    pub(super) fn new(
        config: &ServiceConfig,
        loggers: Vec<BrokerLoggerId>,
        initializing: bool,
    ) -> Self {
        Self {
            auxiliary: AuxiliaryExecution::Idle,
            condition_tracker: ConditionTracker::default(),
            deadline: None,
            descriptor: None,
            epoch: Instant::now(),
            first_start: true,
            goal_revision: 0,
            logger_failures: 0,
            task_signals: OwedTaskAcknowledgements::default(),
            logger_shutdown: LoggerShutdownState::Running,
            lifecycle: None,
            loggers: loggers.into_iter().map(LoggerExecution::new).collect(),
            machine: if initializing {
                StateMachine::initializing(DesiredState::Up)
            } else {
                StateMachine::default()
            },
            next_task: 1,
            overdue: OverdueReports::default(),
            pending_completion: None,
            pending_detach: None,
            pending_signals: VecDeque::new(),
            pending_stop: None,
            status: RuntimeStatus::new(config),
            stop_kill_sent: false,
            shutdown_requested: false,
            tracker: RestartTracker::default(),
            terminal_failure: None,
        }
    }

    pub(super) fn loggers_ready(&self) -> bool {
        self.loggers
            .iter()
            .all(|logger| matches!(logger.state, LoggerExecutionState::Running { .. }))
    }

    pub(super) fn loggers_failed(&self) -> bool {
        self.loggers
            .iter()
            .any(|logger| matches!(logger.state, LoggerExecutionState::Failed))
    }

    pub(super) fn next_deadline(&self) -> Option<TokioInstant> {
        // While loggers shut down only the shutdown state's own deadline is
        // acted on; an elapsed per-logger deadline would wake the loop at
        // once, again and again, until that deadline passes.
        let loggers: &[LoggerExecution] = if self.logger_shutdown.permits_restart() {
            &self.loggers
        } else {
            &[]
        };
        let supervision = earliest(
            self.deadline,
            self.lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.deadline),
        );
        loggers
            .iter()
            .filter_map(|logger| logger.state.deadline())
            .fold(
                self.logger_shutdown.deadline().or(supervision),
                |current, deadline| earliest(current, Some(deadline)),
            )
    }

    /// Whether the lifecycle hook's own deadline has passed.
    pub(super) fn lifecycle_deadline_due(&self) -> bool {
        is_due(
            self.lifecycle
                .as_ref()
                .and_then(|lifecycle| lifecycle.deadline),
        )
    }
}

/// The earlier of two optional deadlines.
fn earliest(first: Option<TokioInstant>, second: Option<TokioInstant>) -> Option<TokioInstant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, second) => first.or(second),
    }
}

/// Whether `deadline` is set and has passed.
pub(super) fn is_due(deadline: Option<TokioInstant>) -> bool {
    deadline.is_some_and(|deadline| deadline <= TokioInstant::now())
}

/// Signal acknowledgements the broker still owes for broker tasks.
///
/// The broker answers every signal request, also for a task whose start
/// failed or whose process it already reaped. A logger, hook, or condition
/// can therefore leave the state that names its task before the answer
/// arrives, so what is owed is tracked apart from that state: one entry per
/// signal sent, retired when its answer arrives. Task identifiers are never
/// reused, so an entry cannot match another task's answer.
#[derive(Default)]
pub(super) struct OwedTaskAcknowledgements {
    tasks: Vec<BrokerTaskId>,
}

impl OwedTaskAcknowledgements {
    /// Signal one task's process group and note the acknowledgement owed.
    pub(super) async fn signal(
        &mut self,
        client: &mut ProcessBrokerClient,
        task: BrokerTaskId,
        signal: ProcessSignal,
    ) -> Result<(), ProcessBrokerError> {
        client
            .signal_task(task, BrokerSignalScope::Group, signal)
            .await?;
        self.tasks.push(task);
        Ok(())
    }

    /// Retire the entry `event` answers, if it is an owed acknowledgement.
    pub(super) fn retire(&mut self, event: &ProcessBrokerEvent) -> bool {
        let (ProcessBrokerEvent::TaskSignalDelivered { task }
        | ProcessBrokerEvent::TaskSignalFailed { task, .. }) = event
        else {
            return false;
        };
        let Some(position) = self.tasks.iter().position(|owed| owed == task) else {
            return false;
        };
        self.tasks.swap_remove(position);
        true
    }

    /// Whether an answer to a signal sent to `task` is still owed.
    pub(super) fn owes(&self, task: BrokerTaskId) -> bool {
        self.tasks.contains(&task)
    }

    /// Number of acknowledgements still owed.
    #[cfg(test)]
    pub(super) const fn len(&self) -> usize {
        self.tasks.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleHookKind {
    Reload,
    Stop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifecycleHookState {
    Starting,
    Running,
    Killing,
    WaitingLifetime,
}

/// One descriptor lifecycle hook and the deadline only it owns.
///
/// The hook keeps its own deadline beside the generation's: a reload runs while
/// the service stays live, so readiness, job control, completion, and backoff
/// keep driving the generation's deadline meanwhile, and neither may overwrite
/// or clear the other's.
pub(super) struct LifecycleExecution {
    pub(super) after: Option<StopCompletion>,
    pub(super) command: Option<ControlCommand>,
    pub(super) deadline: Option<TokioInstant>,
    pub(super) generation: Generation,
    pub(super) kind: LifecycleHookKind,
    pub(super) previous_desired: DesiredState,
    pub(super) response: Option<Response>,
    pub(super) resume_ready: bool,
    pub(super) state: LifecycleHookState,
    pub(super) task: BrokerTaskId,
    pub(super) timeout: Duration,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct DescriptorExecution {
    pub(super) generation: Generation,
    pub(super) launcher_result: Option<ChildResult>,
    pub(super) lifetime_preceded_launcher: bool,
    pub(super) lifetime_result: Option<ChildResult>,
}

impl DescriptorExecution {
    pub(super) const fn new(generation: Generation) -> Self {
        Self {
            generation,
            launcher_result: None,
            lifetime_preceded_launcher: false,
            lifetime_result: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LoggerShutdownTier {
    FileAdapters,
    SharedLogger,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LoggerShutdownState {
    Running,
    Preparing {
        deadline: TokioInstant,
    },
    ClosingInputs {
        deadline: TokioInstant,
    },
    Draining {
        deadline: TokioInstant,
        tier: LoggerShutdownTier,
    },
    Terminating {
        deadline: TokioInstant,
        kill_sent: bool,
        tier: LoggerShutdownTier,
    },
    Complete,
}

impl LoggerShutdownState {
    pub(super) const fn deadline(self) -> Option<TokioInstant> {
        match self {
            Self::Preparing { deadline }
            | Self::ClosingInputs { deadline }
            | Self::Draining { deadline, .. }
            | Self::Terminating { deadline, .. } => Some(deadline),
            Self::Running | Self::Complete => None,
        }
    }

    pub(super) const fn permits_restart(self) -> bool {
        matches!(self, Self::Running | Self::Preparing { .. })
    }
}

/// One logger's most recent failure and where it falls among all of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct LoggerFailure {
    pub(super) result: LoggerLastResult,
    /// Position in the supervisor-wide order of logger failures; a later
    /// failure has a larger value.
    pub(super) order: u64,
    /// Task still awaiting collection after this failure. Its reap is cleanup
    /// of the failed start, so it must not replace the recorded cause; a reap
    /// of any other task is a new failure.
    pub(super) cleanup: Option<BrokerTaskId>,
}

pub(super) struct LoggerExecution {
    pub(super) failure_streak: u32,
    /// Most recent failure, kept until this logger runs again so status can
    /// say why it is not healthy.
    pub(super) last_failure: Option<LoggerFailure>,
    pub(super) logger: BrokerLoggerId,
    pub(super) state: LoggerExecutionState,
}

impl LoggerExecution {
    pub(super) const fn new(logger: BrokerLoggerId) -> Self {
        Self {
            failure_streak: 0,
            last_failure: None,
            logger,
            state: LoggerExecutionState::Down,
        }
    }

    /// Record this logger's newest failure and advance the shared `failures`
    /// count that orders it after every earlier one.
    ///
    /// `cleanup` names the task whose later reap belongs to this failure.
    pub(super) const fn record_failure(
        &mut self,
        failures: &mut u64,
        result: LoggerLastResult,
        cleanup: Option<BrokerTaskId>,
    ) {
        *failures = failures.saturating_add(1);
        self.last_failure = Some(LoggerFailure {
            result,
            order: *failures,
            cleanup,
        });
    }

    /// Whether reaping `task` only completes the failure already recorded.
    pub(super) fn reap_completes_failure(&self, task: BrokerTaskId) -> bool {
        self.last_failure
            .is_some_and(|failure| failure.cleanup == Some(task))
    }
}

/// The most recent failure among loggers which have not run again since.
pub(super) fn latest_logger_failure(loggers: &[LoggerExecution]) -> Option<LoggerLastResult> {
    loggers
        .iter()
        .filter_map(|logger| logger.last_failure)
        .max_by_key(|failure| failure.order)
        .map(|failure| failure.result)
}

#[derive(Clone, Copy, Debug)]
pub(super) enum LoggerExecutionState {
    Down,
    Starting {
        task: BrokerTaskId,
        deadline: TokioInstant,
    },
    Running {
        task: BrokerTaskId,
        started_at: Instant,
    },
    Backoff {
        deadline: TokioInstant,
    },
    Failed,
    Killing {
        task: BrokerTaskId,
        deadline: TokioInstant,
    },
}

impl LoggerExecutionState {
    pub(super) const fn task(self) -> Option<BrokerTaskId> {
        match self {
            Self::Starting { task, .. }
            | Self::Running { task, .. }
            | Self::Killing { task, .. } => Some(task),
            Self::Down | Self::Backoff { .. } | Self::Failed => None,
        }
    }

    pub(super) const fn deadline(self) -> Option<TokioInstant> {
        match self {
            Self::Starting { deadline, .. }
            | Self::Backoff { deadline }
            | Self::Killing { deadline, .. } => Some(deadline),
            Self::Down | Self::Running { .. } | Self::Failed => None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) enum AuxiliaryExecution {
    #[default]
    Idle,
    Starting(BrokerTaskId),
    Running(BrokerTaskId),
    Killing {
        task: BrokerTaskId,
        retry: bool,
    },
    Passed,
    HookStarting(BrokerTaskId),
    HookRunning(BrokerTaskId),
    HookKilling(BrokerTaskId),
}

impl AuxiliaryExecution {
    /// Task this state names, if any.
    pub(super) const fn task(self) -> Option<BrokerTaskId> {
        match self {
            Self::Starting(task)
            | Self::Running(task)
            | Self::Killing { task, .. }
            | Self::HookStarting(task)
            | Self::HookRunning(task)
            | Self::HookKilling(task) => Some(task),
            Self::Idle | Self::Passed => None,
        }
    }

    pub(super) const fn is_idle(self) -> bool {
        matches!(self, Self::Idle | Self::Passed)
    }

    pub(super) const fn is_hook(self) -> bool {
        matches!(
            self,
            Self::HookStarting(_) | Self::HookRunning(_) | Self::HookKilling(_)
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct PendingCompletion {
    pub(super) generation: Generation,
    /// The operator goal revision when the generation completed.
    pub(super) goal_revision: u64,
    pub(super) policy_result: ChildResult,
    pub(super) runtime_seconds: u64,
    pub(super) stop: Option<StopCompletion>,
}
pub(super) struct PendingDetach {
    pub(super) generation: Generation,
    pub(super) previous_desired: DesiredState,
    pub(super) command: ControlCommand,
    pub(super) success: Response,
}

pub(super) struct RuntimeStatus {
    pub(super) command: Vec<String>,
    pub(super) down_since: Option<Instant>,
    pub(super) failures: u64,
    pub(super) last_result: Option<ChildResult>,
    pub(super) last_readiness_failed: bool,
    pub(super) last_start_failed: bool,
    /// Stage and cause of the most recent failed start, published only while
    /// `last_start_failed` holds.
    pub(super) last_spawn_failure: Option<SpawnDetail>,
    pub(super) main_pid: Option<u32>,
    pub(super) main_pid_file: Option<OwnedPidFile>,
    pub(super) main_pid_path: Option<std::path::PathBuf>,
    pub(super) readiness_failed: bool,
    pub(super) started_at: Option<Instant>,
}

impl RuntimeStatus {
    pub(super) fn new(config: &ServiceConfig) -> Self {
        Self {
            command: config.command.clone(),
            down_since: None,
            failures: 0,
            last_result: None,
            last_readiness_failed: false,
            last_start_failed: false,
            last_spawn_failure: None,
            main_pid: None,
            main_pid_file: None,
            main_pid_path: config.pid_files.main.clone(),
            readiness_failed: false,
            started_at: None,
        }
    }

    pub(super) fn publish_main_pid(&mut self, process: ProcessId) -> io::Result<()> {
        let process = u32::try_from(process.get())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "child PID is outside u32"))?;
        self.main_pid_file = self
            .main_pid_path
            .as_deref()
            .map(|path| OwnedPidFile::publish(path, process))
            .transpose()?;
        self.main_pid = Some(process);
        Ok(())
    }

    pub(super) fn clear_main_pid(&mut self) {
        self.main_pid = None;
        self.main_pid_file = None;
    }

    pub(super) fn snapshot(
        &self,
        machine: &StateMachine,
        tracker: &RestartTracker,
        deadline: Option<TokioInstant>,
        loggers: &[LoggerExecution],
    ) -> StatusSnapshot {
        let mut snapshot = StatusSnapshot::from_machine(machine);
        snapshot.supervisor_pid = Some(std::process::id());
        snapshot.main_pid = self.main_pid;
        snapshot.uptime_seconds = self.started_at.map(elapsed_seconds);
        snapshot.down_seconds = self.down_since.map(elapsed_seconds);
        snapshot.starts = tracker.total_starts();
        snapshot.failures = self.failures;
        snapshot.last_result = self.last_result.map(|result| {
            if self.last_readiness_failed {
                LastResult::ReadinessTimeout
            } else if self.last_start_failed {
                LastResult::SpawnFailed
            } else {
                match result {
                    ChildResult::Exited(code) => LastResult::Exited(code),
                    ChildResult::Signaled(signal) => LastResult::Signaled(signal),
                    ChildResult::LifetimeClosed => LastResult::LifetimeClosed,
                    ChildResult::LifetimeFailed => LastResult::LifetimeFailed,
                }
            }
        });
        snapshot.backoff_seconds = if matches!(machine.state(), SupervisorState::Backoff { .. }) {
            deadline.map(|deadline| {
                deadline
                    .saturating_duration_since(TokioInstant::now())
                    .as_secs()
            })
        } else {
            None
        };
        snapshot.last_spawn_failure = if snapshot.last_result == Some(LastResult::SpawnFailed) {
            self.last_spawn_failure
        } else {
            None
        };
        snapshot.logger = logger_status(loggers);
        // A failure is published only as the reason a stage is not running.
        snapshot.logger_last_result = match snapshot.logger {
            LoggerStatus::Starting | LoggerStatus::Backoff | LoggerStatus::Failed => {
                latest_logger_failure(loggers)
            }
            LoggerStatus::NotConfigured | LoggerStatus::Ready => None,
        };
        snapshot.command.clone_from(&self.command);
        snapshot
    }
}
