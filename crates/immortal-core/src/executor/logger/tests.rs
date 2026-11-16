//! Tests for which logger failure status reports.
//!
//! Status promises the most recent failure among loggers which have not run
//! again, and a failed start's cause must survive only the reap of the task
//! that start left behind. These tests drive the same event and timer
//! handlers the executor uses, so a cause recorded for one task cannot stand
//! in for a later task's failure.
//!
//! The later tests cover what the dispatch does for any broker task, logger
//! or not, after its owner let go of it: the acknowledgement of a signal the
//! supervisor sent, and the reap that can follow a failed start or a kill
//! the broker could not confirm.

use std::{error::Error, time::Instant};

use tokio::{net::UnixStream, time::Instant as TokioInstant};

use super::{
    super::{
        events::{handle_broker_event, handle_logger_event},
        prepare::prepare_execution,
        timers::cancel_auxiliary,
    },
    AuxiliaryExecution, BrokerLoggerId, ExecutionContext, ExecutorError, LoggerExecutionState,
    LoggerLastResult, LoggerShutdownState, LoggerShutdownTier, ProcessBrokerClient, ProcessId,
    ProcessSignal, SpawnDetail, SpawnFailure, SpawnStage, handle_due_logger_timers,
    signal_logger_tier,
};
use crate::{
    config::{ServiceConfig, parse_str},
    process::{BrokerTaskId, ChildEvent, ProcessBrokerEvent, ProcessGroupId},
};

const SHARED: usize = 0;
const ADAPTER: usize = 1;

struct Fixture {
    client: ProcessBrokerClient,
    config: ServiceConfig,
    execution: ExecutionContext,
    // Keeps the broker end open so signals the executor sends are accepted.
    _broker: UnixStream,
}

impl Fixture {
    fn new() -> Result<Self, Box<dyn Error>> {
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
    fn begin_start(&mut self, logger: usize, task: u64) -> Result<BrokerTaskId, Box<dyn Error>> {
        let task = BrokerTaskId::new(task).ok_or("invalid test task")?;
        self.logger(logger)?.state = LoggerExecutionState::Starting {
            task,
            deadline: TokioInstant::now(),
        };
        Ok(task)
    }

    fn logger(&mut self, logger: usize) -> Result<&mut super::LoggerExecution, Box<dyn Error>> {
        self.execution
            .loggers
            .get_mut(logger)
            .ok_or_else(|| "missing test logger".into())
    }

    fn handle(&mut self, event: ProcessBrokerEvent) -> Result<(), ExecutorError> {
        handle_logger_event(&self.client, event, &self.config, &mut self.execution)
    }

    fn started(&mut self, task: BrokerTaskId) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskStarted {
            task,
            process: process()?,
            group: ProcessGroupId::try_from(1)?,
        })?)
    }

    fn spawn_failed(&mut self, task: BrokerTaskId, cleanup: bool) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskSpawnFailed {
            task,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
            cleanup_pending: if cleanup { Some(process()?) } else { None },
        })?)
    }

    fn exited(&mut self, task: BrokerTaskId, code: u8) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code,
            },
        })?)
    }

    fn killed(&mut self, task: BrokerTaskId) -> Result<(), Box<dyn Error>> {
        Ok(self.handle(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Signaled {
                pid: process()?,
                signal: 9,
            },
        })?)
    }

    /// The logger failure a status request would publish now.
    fn reported(&self) -> Option<LoggerLastResult> {
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

fn process() -> Result<ProcessId, Box<dyn Error>> {
    ProcessId::new(1).ok_or_else(|| "invalid test process".into())
}

const EXECUTE_FAILED: LoggerLastResult = LoggerLastResult::SpawnFailed(SpawnDetail {
    stage: SpawnStage::Execute,
    failure: SpawnFailure::OperatingSystem,
    os_error: Some(2),
});

const START_TIMED_OUT: LoggerLastResult = LoggerLastResult::SpawnFailed(SpawnDetail {
    stage: SpawnStage::StartupHandshake,
    failure: SpawnFailure::TimedOut,
    os_error: None,
});

/// Fail the shared logger's start and the adapter's run, in the given order.
fn fail_both(fixture: &mut Fixture, shared_first: bool) -> Result<(), Box<dyn Error>> {
    let shared = fixture.begin_start(SHARED, 1)?;
    let adapter = fixture.begin_start(ADAPTER, 2)?;
    fixture.started(adapter)?;
    if shared_first {
        fixture.spawn_failed(shared, false)?;
        fixture.exited(adapter, 3)?;
    } else {
        fixture.exited(adapter, 3)?;
        fixture.spawn_failed(shared, false)?;
    }
    Ok(())
}

/// Regression: status named the first logger with a failure, so an older
/// shared-logger failure hid a later adapter failure.
#[tokio::test]
async fn status_reports_the_later_failure_whichever_logger_it_belongs_to()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fail_both(&mut fixture, true)?;
    assert_eq!(fixture.reported(), Some(LoggerLastResult::Exited(3)));

    let mut fixture = Fixture::new()?;
    fail_both(&mut fixture, false)?;
    assert_eq!(fixture.reported(), Some(EXECUTE_FAILED));
    Ok(())
}

#[tokio::test]
async fn a_recovered_logger_stops_being_reported_and_the_other_remains()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fail_both(&mut fixture, true)?;

    let adapter = fixture.begin_start(ADAPTER, 3)?;
    fixture.started(adapter)?;
    assert_eq!(
        fixture.reported(),
        Some(EXECUTE_FAILED),
        "the adapter runs again, so only the shared logger's failure remains"
    );

    let shared = fixture.begin_start(SHARED, 4)?;
    fixture.started(shared)?;
    assert_eq!(fixture.reported(), None);
    Ok(())
}

#[tokio::test]
async fn the_reap_of_a_failed_start_keeps_the_spawn_cause() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = fixture.begin_start(SHARED, 1)?;
    fixture.spawn_failed(task, true)?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { .. }
    ));

    fixture.killed(task)?;

    assert_eq!(fixture.reported(), Some(EXECUTE_FAILED));
    Ok(())
}

/// Regression: any reap kept an earlier spawn failure, so a later task that
/// exited on its own was reported as the previous task's failed start.
#[tokio::test]
async fn a_later_tasks_exit_replaces_an_earlier_spawn_failure() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let first = fixture.begin_start(SHARED, 1)?;
    fixture.spawn_failed(first, true)?;
    fixture.killed(first)?;

    let second = fixture.begin_start(SHARED, 2)?;
    fixture.exited(second, 1)?;

    assert_eq!(fixture.reported(), Some(LoggerLastResult::Exited(1)));
    Ok(())
}

/// Regression: a start that reached its deadline recorded nothing, so the
/// kill's reap, even after a late start report, kept the failure of the task
/// before it.
#[tokio::test]
async fn a_start_deadline_is_reported_instead_of_the_previous_tasks_failure()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let adapter = fixture.begin_start(ADAPTER, 1)?;
    fixture.started(adapter)?;
    let first = fixture.begin_start(SHARED, 2)?;
    fixture.spawn_failed(first, false)?;
    assert_eq!(fixture.reported(), Some(EXECUTE_FAILED));

    // The next attempt is already past its start deadline.
    let second = fixture.begin_start(SHARED, 3)?;
    handle_due_logger_timers(&mut fixture.client, &mut fixture.execution).await?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { task, .. } if task == second
    ));
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));

    fixture.started(second)?;
    assert_eq!(
        fixture.reported(),
        Some(START_TIMED_OUT),
        "a start reported after the kill does not make the logger healthy"
    );

    fixture.killed(second)?;
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    assert!(matches!(
        fixture.logger(ADAPTER)?.state,
        LoggerExecutionState::Running { started_at, .. } if started_at <= Instant::now()
    ));
    Ok(())
}

/// Put the shared logger's start past its deadline and let the timer kill it.
async fn time_out_shared_start(fixture: &mut Fixture) -> Result<BrokerTaskId, Box<dyn Error>> {
    let task = fixture.begin_start(SHARED, 7)?;
    handle_due_logger_timers(&mut fixture.client, &mut fixture.execution).await?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { .. }
    ));
    Ok(task)
}

/// Regression: the broker can report a failed start after the supervisor's
/// own start deadline killed the task. That report was an unexpected event,
/// which ended supervision instead of retrying the logger.
#[tokio::test]
async fn a_spawn_failure_reported_after_the_start_deadline_retries_the_logger()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;

    fixture.spawn_failed(task, false)?;

    assert!(
        matches!(
            fixture.logger(SHARED)?.state,
            LoggerExecutionState::Backoff { .. }
        ),
        "no child is left to reap, so the retry is scheduled now"
    );
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    Ok(())
}

#[tokio::test]
async fn a_late_spawn_failure_with_a_child_to_collect_waits_for_its_reap()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;

    fixture.spawn_failed(task, true)?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Killing { .. }
    ));

    fixture.killed(task)?;
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Backoff { .. }
    ));
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    Ok(())
}

#[tokio::test]
async fn a_late_spawn_failure_during_shutdown_leaves_the_logger_down() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture.execution.logger_shutdown = LoggerShutdownState::Complete;

    fixture.spawn_failed(task, false)?;

    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Down
    ));
    Ok(())
}

/// Accepting a late failure for a known task must not accept one for a task
/// no logger owns.
#[tokio::test]
async fn a_spawn_failure_for_an_unowned_task_stays_a_protocol_fault() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    time_out_shared_start(&mut fixture).await?;
    let unowned = BrokerTaskId::new(99).ok_or("invalid test task")?;

    let outcome = fixture.handle(ProcessBrokerEvent::TaskSpawnFailed {
        task: unowned,
        stage: SpawnStage::Execute,
        failure: SpawnFailure::OperatingSystem,
        os_error: Some(2),
        cleanup_pending: None,
    });

    assert!(matches!(
        outcome,
        Err(ExecutorError::UnexpectedBrokerEvent(_))
    ));
    Ok(())
}

impl Fixture {
    /// Deliver a broker event through the executor's dispatch, as the event
    /// loop does, so routing by task ownership is part of what is tested.
    async fn dispatch(&mut self, event: ProcessBrokerEvent) -> Result<(), Box<dyn Error>> {
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

/// Regression: after a late start failure the logger no longer named the
/// task, so the broker's answer to the supervisor's own kill was routed as an
/// event nobody owned and ended supervision.
#[tokio::test]
async fn the_kill_acknowledgement_is_consumed_after_a_late_spawn_failure()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSpawnFailed {
            task,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
            cleanup_pending: None,
        })
        .await?;

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Backoff { .. }
    ));
    assert_eq!(fixture.execution.task_signals.len(), 0);
    assert_eq!(fixture.reported(), Some(START_TIMED_OUT));
    Ok(())
}

/// The process can die on its own before the broker handles the kill, so the
/// acknowledgement trails the reap.
#[tokio::test]
async fn the_kill_acknowledgement_is_consumed_after_the_reap() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code: 1,
            },
        })
        .await?;

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert_eq!(fixture.execution.task_signals.len(), 0);
    Ok(())
}

/// Each signal is acknowledged once: the acknowledgement of the kill is
/// consumed, and a second one answers nothing this supervisor sent.
#[tokio::test]
async fn an_acknowledgement_that_is_not_owed_stays_a_protocol_fault() -> Result<(), Box<dyn Error>>
{
    let mut fixture = Fixture::new()?;
    let task = time_out_shared_start(&mut fixture).await?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await?;

    let repeated = fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await;

    assert!(repeated.is_err());
    Ok(())
}

/// A shutdown tier sends TERM and CONT, so two acknowledgements are owed for
/// each logger and both are consumed, also after the logger was reaped.
#[tokio::test]
async fn shutdown_signals_owe_one_acknowledgement_each() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    let task = fixture.begin_start(SHARED, 1)?;
    fixture.started(task)?;
    fixture.execution.logger_shutdown = LoggerShutdownState::Complete;
    signal_logger_tier(
        &mut fixture.client,
        &mut fixture.execution,
        LoggerShutdownTier::SharedLogger,
        ProcessSignal::Terminate,
    )
    .await?;
    assert_eq!(fixture.execution.task_signals.len(), 2);

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalDelivered { task })
        .await?;
    fixture.killed(task)?;
    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert_eq!(fixture.execution.task_signals.len(), 0);
    assert!(matches!(
        fixture.logger(SHARED)?.state,
        LoggerExecutionState::Down
    ));
    Ok(())
}

/// Regression: only logger tasks had their acknowledgements tracked. A start
/// condition that was cancelled and reaped before the broker answered its
/// kill left an answer nobody owned, which ended supervision.
#[tokio::test]
async fn a_cancelled_conditions_kill_acknowledgement_is_consumed_after_its_reap()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(
        "version: 2\ncommand: [/bin/true]\nstart_condition:\n  command: [/bin/true]\n  \
         timeout_seconds: 10\n",
    )?;
    let task = BrokerTaskId::new(40).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Running(task);
    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;
    assert_eq!(fixture.execution.task_signals.len(), 1);

    fixture
        .dispatch(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Signaled {
                pid: process()?,
                signal: 9,
            },
        })
        .await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;
    assert_eq!(fixture.execution.task_signals.len(), 0);

    let repeated = fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await;
    assert!(repeated.is_err(), "an answer that is not owed is a fault");
    Ok(())
}

/// The state that still names the task acts on its acknowledgement: a kill
/// the broker could not deliver ends the cancelled condition at once.
#[tokio::test]
async fn an_owned_acknowledgement_still_reaches_its_task_state() -> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(
        "version: 2\ncommand: [/bin/true]\nstart_condition:\n  command: [/bin/true]\n  \
         timeout_seconds: 10\n",
    )?;
    let task = BrokerTaskId::new(41).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Running(task);
    cancel_auxiliary(&mut fixture.client, &mut fixture.execution).await?;

    fixture
        .dispatch(ProcessBrokerEvent::TaskSignalFailed {
            task,
            os_error: Some(3),
        })
        .await?;

    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);
    assert_eq!(fixture.execution.task_signals.len(), 0);

    // Regression: a kill the broker could not confirm may still be delivered,
    // so the reap can follow after the condition was given up. It ended
    // supervision as an event nobody owned.
    let reap = ProcessBrokerEvent::TaskChild {
        task,
        event: ChildEvent::Signaled {
            pid: process()?,
            signal: 9,
        },
    };
    fixture.dispatch(reap.clone()).await?;
    assert!(fixture.execution.released_tasks.is_empty());
    assert!(
        fixture.dispatch(reap).await.is_err(),
        "a task is reaped once; a second reap is a fault"
    );
    Ok(())
}

/// Regression: a start that failed after forking leaves a child to collect.
/// The condition retried at once and the child's reap then ended supervision.
#[tokio::test]
async fn the_reap_of_a_condition_whose_start_failed_is_stale_not_a_fault()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(
        "version: 2\ncommand: [/bin/true]\nstart_condition:\n  command: [/bin/true]\n  \
         timeout_seconds: 10\n",
    )?;
    let task = BrokerTaskId::new(42).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Starting(task);

    fixture
        .dispatch(ProcessBrokerEvent::TaskSpawnFailed {
            task,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
            cleanup_pending: Some(process()?),
        })
        .await?;
    assert_eq!(fixture.execution.auxiliary, AuxiliaryExecution::Idle);

    fixture
        .dispatch(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code: 127,
            },
        })
        .await?;
    assert!(fixture.execution.released_tasks.is_empty());
    Ok(())
}

/// A failed start with nothing to collect is followed by no reap, so one
/// that arrives anyway answers nothing and stays a fault.
#[tokio::test]
async fn a_reap_after_a_failed_start_without_a_child_stays_a_protocol_fault()
-> Result<(), Box<dyn Error>> {
    let mut fixture = Fixture::new()?;
    fixture.config = parse_str(
        "version: 2\ncommand: [/bin/true]\nstart_condition:\n  command: [/bin/true]\n  \
         timeout_seconds: 10\n",
    )?;
    let task = BrokerTaskId::new(43).ok_or("invalid test task")?;
    fixture.execution.auxiliary = AuxiliaryExecution::Starting(task);
    fixture
        .dispatch(ProcessBrokerEvent::TaskSpawnFailed {
            task,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            os_error: Some(2),
            cleanup_pending: None,
        })
        .await?;

    let reap = fixture
        .dispatch(ProcessBrokerEvent::TaskChild {
            task,
            event: ChildEvent::Exited {
                pid: process()?,
                code: 127,
            },
        })
        .await;
    assert!(reap.is_err());
    Ok(())
}
