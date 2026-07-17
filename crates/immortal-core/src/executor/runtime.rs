//! Main broker-driven supervision loop.
//!
//! The runtime child owns the single-threaded Tokio executor after broker
//! startup has completed. It waits for broker readiness, installs termination
//! signal handling, optionally binds the authenticated control server, then
//! serializes timers, broker events, operator requests, and shutdown into one
//! lifecycle state machine. Every exit path asks the broker to shut down and is
//! followed by a blocking broker reap in the caller.

use std::{io, sync::Arc};

use tokio::{
    runtime::Builder,
    sync::{mpsc, watch},
    time::{Instant as TokioInstant, timeout},
};

use super::{
    BROKER_EVENT_TIMEOUT, BrokerLoggerId, ControlCommand, ControlListener, ControlSetup,
    DEFAULT_MAX_CONTROL_CLIENTS, DesiredState, ExecutionContext, ExecutorError, ExecutorEvent,
    OwnedPidFile, PreparedExecution, PreparedLaunch, ProcessBrokerClient, ProcessBrokerEvent,
    ProcessMode, ServiceConfig, StartupReporter, StateMachine, SupervisionOutcome, SupervisorState,
    TerminationSignals, advance_childless_state, advance_logger_shutdown, apply_control_command,
    begin_descriptor_shutdown, begin_supervisor_shutdown, broker_lifetime_plan, cancel_auxiliary,
    fail_childless_start_on_logger_exhaustion, handle_broker_event, handle_due_logger_timers,
    handle_executor_timer, next_executor_event, outcome, reap_broker, respond_abandoned_signals,
    run_control_server, shutdown_broker, start_down_loggers, start_process_broker_with_logging,
};

pub(super) fn run_prepared(
    config: &ServiceConfig,
    control: Option<ControlSetup>,
    prepared: PreparedLaunch,
    startup: &mut StartupReporter,
) -> Result<SupervisionOutcome, ExecutorError> {
    let _supervisor_pid_file = config
        .pid_files
        .supervisor
        .as_deref()
        .map(|path| OwnedPidFile::publish(path, std::process::id()))
        .transpose()?;
    let lifetime_cleanup = broker_lifetime_plan(config, &prepared.execution)?;
    let endpoint = start_process_broker_with_logging(prepared.logging, lifetime_cleanup)?;
    let commands = prepared.execution;
    let loggers = prepared.loggers;
    let broker_process = endpoint.process();
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let execution = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        wait_for_ready(&mut client).await?;
        let mut signals = TerminationSignals::new()?;
        let outcome = if let Some(setup) = control {
            drive_controlled_service(
                &mut client,
                commands,
                loggers,
                config,
                &mut signals,
                setup,
                startup,
            )
            .await
        } else {
            startup.notify_ready()?;
            drive_service(
                &mut client,
                commands,
                loggers,
                config,
                &mut signals,
                None,
                None,
            )
            .await
        };
        let shutdown = shutdown_broker(&mut client).await;
        match (outcome, shutdown) {
            (Ok(outcome), Ok(())) => Ok(outcome),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    });
    drop(runtime);
    let broker_exit = reap_broker(broker_process);
    match (execution, broker_exit) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) | (Ok(_), Err(error)) => Err(error),
    }
}
pub(super) async fn drive_controlled_service(
    client: &mut ProcessBrokerClient,
    commands: PreparedExecution,
    loggers: Vec<BrokerLoggerId>,
    config: &ServiceConfig,
    signals: &mut TerminationSignals,
    setup: ControlSetup,
    startup: &mut StartupReporter,
) -> Result<SupervisionOutcome, ExecutorError> {
    let listener = Arc::new(ControlListener::bind(
        setup.owner.socket(),
        DEFAULT_MAX_CONTROL_CLIENTS,
    )?);
    let (command_sender, mut control_commands) = mpsc::channel(DEFAULT_MAX_CONTROL_CLIENTS);
    let (shutdown_sender, shutdown_receiver) = watch::channel(false);
    let server_listener = Arc::clone(&listener);
    let server = tokio::spawn(run_control_server(
        server_listener,
        command_sender,
        shutdown_receiver,
    ));
    startup.notify_ready()?;
    let outcome = drive_service(
        client,
        commands,
        loggers,
        config,
        signals,
        Some(&mut control_commands),
        Some(setup.service_name.as_str()),
    )
    .await;
    let _ = shutdown_sender.send(true);
    let server_result = server
        .await
        .map_err(|error| io::Error::other(format!("control server task failed: {error}")))?;
    drop(listener);
    drop(setup.owner);
    match (outcome, server_result) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(io::Error::other(error).into()),
    }
}

pub(super) async fn wait_for_ready(client: &mut ProcessBrokerClient) -> Result<(), ExecutorError> {
    match timeout(BROKER_EVENT_TIMEOUT, client.next_event()).await {
        Ok(Ok(ProcessBrokerEvent::Ready)) => Ok(()),
        Ok(Ok(event)) => Err(ExecutorError::UnexpectedBrokerEvent(event)),
        Ok(Err(error)) => Err(error.into()),
        Err(_) => Err(ExecutorError::BrokerTimedOut("startup")),
    }
}

pub(super) async fn drive_service(
    client: &mut ProcessBrokerClient,
    commands: PreparedExecution,
    loggers: Vec<BrokerLoggerId>,
    config: &ServiceConfig,
    signals: &mut TerminationSignals,
    mut controls: Option<&mut mpsc::Receiver<ControlCommand>>,
    service_name: Option<&str>,
) -> Result<SupervisionOutcome, ExecutorError> {
    let mut execution = ExecutionContext::new(config, loggers, controls.is_some());
    start_down_loggers(client, &mut execution).await?;

    loop {
        if execution.shutdown_requested && execution.lifecycle.is_none() {
            begin_descriptor_shutdown(client, &commands, config, &mut execution).await?;
            execution.shutdown_requested = false;
            continue;
        }
        if execution.machine.state() == SupervisorState::Initializing && execution.loggers_ready() {
            execution.machine.initialized()?;
            continue;
        }
        if fail_childless_start_on_logger_exhaustion(client, &mut execution).await? {
            continue;
        }
        if execution.auxiliary.is_idle()
            && execution.lifecycle.is_none()
            && (execution.loggers_ready()
                || matches!(
                    execution.machine.desired(),
                    DesiredState::Halt | DesiredState::Exit
                ))
            && advance_childless_state(
                &mut execution.machine,
                &mut execution.first_start,
                config.start_delay_seconds,
                &mut execution.deadline,
            )?
        {
            continue;
        }
        let lifecycle_finished = supervision_finished(
            &execution.machine,
            controls.is_some(),
            execution.first_start,
        );
        if advance_logger_shutdown(client, lifecycle_finished, &mut execution).await? {
            respond_abandoned_signals(&mut execution.pending_signals);
            return Ok(outcome(
                &execution.machine,
                &execution.tracker,
                &execution.status,
                execution.terminal_failure,
            ));
        }

        match next_executor_event(client, signals, &mut controls, execution.next_deadline()).await?
        {
            ExecutorEvent::Timer => {
                handle_due_logger_timers(client, &mut execution).await?;
                if execution
                    .deadline
                    .is_some_and(|deadline| deadline <= TokioInstant::now())
                {
                    handle_executor_timer(client, &commands, config, &mut execution).await?;
                }
            }
            ExecutorEvent::Shutdown => {
                cancel_auxiliary(client, &mut execution).await?;
                if config.process_mode == ProcessMode::DescriptorTracking
                    && execution.machine.state().live_generation().is_some()
                {
                    if execution.lifecycle.is_some() {
                        execution.shutdown_requested = true;
                    } else {
                        begin_descriptor_shutdown(client, &commands, config, &mut execution)
                            .await?;
                    }
                } else {
                    begin_supervisor_shutdown(
                        client,
                        &mut execution.machine,
                        &mut execution.pending_stop,
                        &mut execution.deadline,
                        &mut execution.stop_kill_sent,
                        &mut execution.pending_signals,
                    )
                    .await?;
                }
            }
            ExecutorEvent::Control(command) => {
                let name = service_name.ok_or(ExecutorError::ControlServerStopped)?;
                apply_control_command(client, name, command, &commands, config, &mut execution)
                    .await?;
            }
            ExecutorEvent::ControlClosed => return Err(ExecutorError::ControlServerStopped),
            ExecutorEvent::Broker(event) => {
                handle_broker_event(client, event, &commands, config, &mut execution).await?;
            }
        }
    }
}
pub(super) fn supervision_finished(
    machine: &StateMachine,
    controlled: bool,
    first_start: bool,
) -> bool {
    machine.state() == SupervisorState::Exited
        || (!controlled
            && (matches!(machine.state(), SupervisorState::Failed(_))
                || (!first_start && machine.state() == SupervisorState::Down)))
}
