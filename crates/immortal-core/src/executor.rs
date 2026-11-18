//! Single-owner service supervision driven through the process broker.
//!
//! Preparation resolves every fallible command and credential input before a
//! broker or Tokio runtime exists. The executor then owns lifecycle state,
//! logger stages, deadlines, and serialized control commands while the
//! pre-runtime broker exclusively owns Unix children and wait operations.
//! Checked daemon startup is completed before Tokio starts, and every return
//! path shuts down broker-owned process groups, drains logging, and reaps the
//! broker before reporting a terminal outcome.
//!
//! This facade keeps `immortal_core::executor` as the only public path while
//! private children own the cohesive pieces of the machinery: `outcome` and
//! `error` own the public result contract, `foreground` and `daemon` own entry
//! points, `startup` owns control ownership and daemon readiness reporting,
//! `prepare` owns command and logging materialization, `runtime` owns the main
//! broker-driven loop, `context` owns mutable lifecycle state, `logger` owns
//! logger restart and shutdown tiers, `timers` owns deadline transitions,
//! `overdue` owns waiting for a late broker answer instead of giving up,
//! `readiness` owns readiness failures that arrive while the generation is
//! paused or a lifecycle hook runs,
//! `control` owns authenticated requests and signal conversion, `events`,
//! `completion`, and `auxiliary` own broker event state transitions, and
//! `broker`/`utilities` own broker teardown and shared time/backoff helpers.
//! Every child is private; public items are re-exported below to preserve the
//! established canonical API.

mod auxiliary;
mod broker;
mod completion;
mod context;
mod control;
mod daemon;
mod error;
mod events;
mod foreground;
mod logger;
mod outcome;
mod overdue;
mod prepare;
mod readiness;
mod runtime;
mod startup;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod timers;
mod utilities;

pub use self::daemon::run_daemon;
pub use self::error::ExecutorError;
pub use self::foreground::{run_foreground, run_foreground_controlled};
pub use self::outcome::{DaemonRunOutcome, SupervisionOutcome};

const BROKER_EVENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
/// How long the supervisor waits for each event while the broker shuts down.
///
/// A fail-closed shutdown stays silent for the broker's whole bound before it
/// answers, so the wait must outlast that bound by an ordinary event timeout;
/// otherwise the supervisor reports a timeout instead of the broker's answer.
const BROKER_SHUTDOWN_TIMEOUT: std::time::Duration =
    crate::process::BROKER_SHUTDOWN_BOUND.saturating_add(BROKER_EVENT_TIMEOUT);
const CHILD_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const BROKER_REAP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
const DAEMON_STARTUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);
const SERVICE_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const LOGGER_PREPARE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const LOGGER_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(5);
const LOGGER_STOP_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
const SPAWN_FAILURE_EXIT: u8 = 127;

use self::{
    auxiliary::handle_auxiliary_event,
    broker::{reap_broker, shutdown_broker},
    completion::{
        LifetimeResult, complete_generation, finish_completion, finish_descriptor_generation,
        finish_detach, finish_signal_request, handle_lifetime_event, handle_service_child_event,
        outcome, publish_readiness,
    },
    context::{
        AuxiliaryExecution, DescriptorExecution, ExecutionContext, ExecutorEvent,
        LifecycleExecution, LifecycleHookKind, LifecycleHookState, LoggerExecution,
        LoggerExecutionState, LoggerShutdownState, LoggerShutdownTier, OwedTaskAcknowledgements,
        PendingCompletion, PendingDetach, PendingSignal, RuntimeStatus, is_due,
    },
    control::{
        LifecycleHookRequest, apply_control_command, begin_descriptor_shutdown,
        begin_lifecycle_hook, begin_requested_shutdown, begin_supervisor_shutdown,
        next_executor_event, request_group_stop,
    },
    events::{
        finish_lifecycle_failure, handle_broker_event, handle_readiness_failure, respond_lifecycle,
    },
    logger::{
        advance_logger_shutdown, fail_childless_start_on_logger_exhaustion,
        handle_due_logger_timers, logger_status, logger_tier_is_down, schedule_logger_restart,
        start_down_loggers,
    },
    overdue::{OverdueReports, ending, keep_killing_generation, keep_killing_task, keep_waiting},
    prepare::{PreparedExecution, PreparedLaunch, broker_lifetime_plan, prepare_execution},
    readiness::{act_on_recorded_readiness_failure, handle_readiness_timer},
    runtime::run_prepared,
    startup::{ControlSetup, StartupReporter},
    timers::{
        advance_childless_state, allocate_task, cancel_auxiliary, handle_executor_timer,
        handle_lifecycle_timer,
    },
    utilities::{elapsed_seconds, jittered_backoff, jittered_backoff_seed},
};
use crate::{
    config::{
        FileLogConfig, LoggerRestartConfig, ProcessMode, ReadinessMode, RestartPolicy,
        ServiceConfig, StartConditionConfig,
    },
    control::{
        ControlCommand, ControlEffect, ControlListener, DEFAULT_MAX_CONTROL_CLIENTS, Operation,
        Response, ResponseCode, Signal, SignalScope, StopCompletion, decide_request,
        run_control_server,
    },
    logging::LoggingPlan,
    pid_file::OwnedPidFile,
    process::{
        BrokerFileRoute, BrokerLifetimePlan, BrokerLoggerId, BrokerLoggingPlan, BrokerSignalScope,
        BrokerTaskId, ChildEvent, DaemonError, DaemonStartup, Daemonized, ProcessBrokerClient,
        ProcessBrokerError, ProcessBrokerEvent, ProcessCommand, ProcessGroupId, ProcessId,
        ProcessSignal, daemonize, reap_any_event, start_process_broker_with_logging,
        wait_for_event,
    },
    runtime::RuntimeOwner,
    shutdown::TerminationSignals,
    status::{LastResult, LoggerStatus, StatusSnapshot},
    supervisor::{
        ChildResult, ConditionTracker, DesiredState, FailureReason, Generation, RestartDecision,
        RestartTracker, StateMachine, SupervisorState, TransitionError,
    },
};

#[cfg(test)]
use self::{logger::next_logger_shutdown_tier, runtime::supervision_finished};
