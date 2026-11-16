//! Shared fixture for the executor's broker-event tests.
//!
//! The fixture owns an execution context, a broker client on a socket pair,
//! and the configuration, and delivers synthetic broker events through the
//! same handlers the event loop uses, so what the tests prove includes how
//! an event is routed to the task that owns it. The socket pair keeps the
//! broker end open, so signals the executor sends are accepted.

use std::{error::Error, time::Duration};

use tokio::{net::UnixStream, time::Instant as TokioInstant};

use super::{
    AuxiliaryExecution, BrokerLoggerId, DesiredState, ExecutionContext, ExecutorError, Generation,
    LifecycleExecution, LifecycleHookKind, LifecycleHookState, LoggerExecutionState,
    ProcessBrokerClient, ProcessId,
    events::{handle_broker_event, handle_logger_event},
    handle_due_logger_timers,
    prepare::prepare_execution,
    timers::cancel_auxiliary,
};
use crate::{
    config::{ServiceConfig, parse_str},
    process::{
        BrokerTaskId, ChildEvent, ProcessBrokerEvent, ProcessGroupId, SpawnFailure, SpawnStage,
    },
    status::{LoggerLastResult, SpawnDetail},
};

pub(super) const SHARED: usize = 0;

pub(super) const ADAPTER: usize = 1;

pub(super) struct Fixture {
    pub(super) client: ProcessBrokerClient,
    pub(super) config: ServiceConfig,
    pub(super) execution: ExecutionContext,
    // Keeps the broker end open so signals the executor sends are accepted.
    _broker: UnixStream,
}

impl Fixture {
    pub(super) fn new() -> Result<Self, Box<dyn Error>> {
        let config = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
        let (supervisor, broker) = UnixStream::pair()?;
        let loggers = vec![BrokerLoggerId::shared_logger(), BrokerLoggerId::new(0, 0)];
        Ok(Self {
            client: ProcessBrokerClient::for_test(supervisor, process()?),
            execution: ExecutionContext::new(&config, loggers, false),
            config,
            _broker: broker,
        })
    }

    /// Put one logger in `Starting` for a new task, as a spawn request does.
    pub(super) fn begin_start(
        &mut self,
        logger: usize,
        task: u64,
    ) -> Result<BrokerTaskId, Box<dyn Error>> {
        let task = BrokerTaskId::new(task).ok_or("invalid test task")?;
        self.logger(logger)?.state = LoggerExecutionState::Starting {
            task,
            deadline: TokioInstant::now(),
        };
        Ok(task)
    }

    pub(super) fn logger(
        &mut self,
        logger: usize,
    ) -> Result<&mut super::LoggerExecution, Box<dyn Error>> {
        self.execution
            .loggers
            .get_mut(logger)
            .ok_or_else(|| "missing test logger".into())
    }

    pub(super) fn handle(&mut self, event: ProcessBrokerEvent) -> Result<(), ExecutorError> {
        handle_logger_event(&self.client, event, &self.config, &mut self.execution)
    }

    pub(super) fn started(&mut self, task: BrokerTaskId) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskStarted {
            task,
            process: process()?,
            group: ProcessGroupId::try_from(1)?,
        })?)
    }

    pub(super) fn spawn_failed(
        &mut self,
        task: BrokerTaskId,
        cleanup: bool,
    ) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskSpawnFailed {
            task,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
            cleanup_pending: if cleanup { Some(process()?) } else { None },
        })?)
    }

    pub(super) fn exited(&mut self, task: BrokerTaskId, code: u8) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code,
            },
        })?)
    }

    pub(super) fn killed(&mut self, task: BrokerTaskId) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Signaled {
                pid: process()?,
                signal: 9,
            },
        })?)
    }

    /// The logger failure a status request would publish now.
    pub(super) fn reported(&self) -> Option<LoggerLastResult> {
        self.execution
            .status
            .snapshot(
                &self.execution.machine,
                &self.execution.tracker,
                None,
                &self.execution.loggers,
            )
            .logger_last_result
    }
}

pub(super) fn process() -> Result<ProcessId, Box<dyn Error>> {
    ProcessId::new(1).ok_or_else(|| "invalid test process".into())
}

pub(super) const EXECUTE_FAILED: LoggerLastResult = LoggerLastResult::SpawnFailed(SpawnDetail {
    stage: SpawnStage::Execute,
    failure: SpawnFailure::OperatingSystem,
    os_error: Some(2),
});

pub(super) const START_TIMED_OUT: LoggerLastResult = LoggerLastResult::SpawnFailed(SpawnDetail {
    stage: SpawnStage::StartupHandshake,
    failure: SpawnFailure::TimedOut,
    os_error: None,
});

/// Put the shared logger's start past its deadline and let the timer kill it.
pub(super) async fn time_out_shared_start(
    fixture: &mut Fixture,
) -> Result<BrokerTaskId, Box<dyn Error>> {
    let task = fixture.begin_start(SHARED, 7)?;
    handle_due_logger_timers(&mut fixture.client, &mut fixture.execution).await?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { .. }
    ));
    Ok(task)
}

impl Fixture {
    /// Deliver a broker event through the executor's dispatch, as the event
    /// loop does, so routing by task ownership is part of what is tested.
    pub(super) async fn dispatch(
        &mut self,
        event: ProcessBrokerEvent,
    ) -> Result<(), Box<dyn Error>> {
        let prepared = prepare_execution(&self.config, false)?;
        Ok(handle_broker_event(
            &mut self.client,
            event,
            &prepared.execution,
            &self.config,
            &mut self.execution,
        )
        .await?)
    }
}

pub(super) const CONDITION: &str = "version: 2\ncommand: [/bin/true]\nstart_condition:\n  command: [/bin/true]\n  \
                         timeout_seconds: 10\n";

pub(super) const POST_EXIT: &str = "version: 2\ncommand: [/bin/true]\npost_exit:\n  command: [/bin/true]\n  \
                         timeout_seconds: 10\n";

pub(super) fn reap(task: BrokerTaskId) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::TaskChild {
        task,
        event: ChildEvent::Signaled {
            pid: process()?,
            signal: 9,
        },
    })
}

pub(super) fn failed_start(
    task: BrokerTaskId,
    child: bool,
) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::TaskSpawnFailed {
        task,
        stage: SpawnStage::Execute,
        failure: SpawnFailure::OperatingSystem,
        os_error: Some(2),
        cleanup_pending: if child { Some(process()?) } else { None },
    })
}

pub(super) const fn kill_failed(task: BrokerTaskId) -> ProcessBrokerEvent {
    ProcessBrokerEvent::TaskSignalFailed {
        task,
        os_error: Some(3),
    }
}

/// A fixture whose start condition was running and has just been cancelled.
pub(super) async fn cancelled_condition(
    task: u64,
) -> Result<(Fixture, BrokerTaskId), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(CONDITION)?;
    let task = BrokerTaskId::new(task).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Running(task);
    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;
    assert_eq!(fixture.execution.task_signals.len(), 1);
    Ok((fixture, task))
}

pub(super) fn stopped(task: BrokerTaskId) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::TaskChild {
        task,
        event: ChildEvent::Stopped {
            pid: process()?,
            signal: 19,
        },
    })
}

pub(super) fn continued(task: BrokerTaskId) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::TaskChild {
        task,
        event: ChildEvent::Continued { pid: process()? },
    })
}

pub(super) fn reload_hook(task: BrokerTaskId, state: LifecycleHookState) -> LifecycleExecution {
    LifecycleExecution {
        after: None,
        command: None,
        generation: Generation::FIRST,
        kind: LifecycleHookKind::Reload,
        previous_desired: DesiredState::Up,
        response: None,
        resume_ready: true,
        state,
        task,
        timeout: Duration::from_secs(10),
    }
}
