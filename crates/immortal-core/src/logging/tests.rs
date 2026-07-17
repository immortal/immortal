//! Facade-spanning logging behavior tests.

use std::error::Error;

use super::{
    BackpressurePolicy, LoggingPlan, LoggingRuntime, LoggingShutdown, LoggingShutdownEffect,
    OutputStream, PipelineHealth,
};
use crate::{
    config::parse_str,
    supervisor::{FailureReason, Generation, SupervisorState},
};

#[test]
fn file_and_logger_normalize_to_combined_route_and_shared_sink() -> Result<(), Box<dyn Error>> {
    let config = parse_str(
        "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/out.log\nlogger: [/usr/bin/logger, -t, service]\n",
    )?;
    let plan = LoggingPlan::from_config(&config.logging)?;
    assert_eq!(plan.local_files.len(), 1);
    let route = plan.local_files.first().ok_or("file route missing")?;
    assert_eq!(route.stream, OutputStream::Combined);
    assert_eq!(route.backpressure, BackpressurePolicy::LosslessBlock);
    assert_eq!(
        plan.logger,
        Some(vec![
            "/usr/bin/logger".to_owned(),
            "-t".to_owned(),
            "service".to_owned()
        ])
    );
    Ok(())
}

#[test]
fn explicit_streams_create_independent_local_routes() -> Result<(), Box<dyn Error>> {
    let config = parse_str(
        "version: 2\ncommand: [/bin/true]\nlog:\n  stdout:\n    file: /tmp/out.log\n  stderr:\n    file: /tmp/err.log\n",
    )?;
    let plan = LoggingPlan::from_config(&config.logging)?;
    assert_eq!(plan.local_files.len(), 2);
    assert_eq!(
        plan.local_files.first().map(|value| value.stream),
        Some(OutputStream::Stdout)
    );
    assert_eq!(
        plan.local_files.get(1).map(|value| value.stream),
        Some(OutputStream::Stderr)
    );
    assert_eq!(plan.logger, None);
    Ok(())
}

#[test]
fn combined_route_covers_both_child_streams() -> Result<(), Box<dyn Error>> {
    let config = parse_str("version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/out.log\n")?;
    let mut runtime = LoggingRuntime::new(LoggingPlan::from_config(&config.logging)?);
    let combined = runtime
        .pipe(OutputStream::Combined)
        .ok_or("combined pipe missing")?;
    assert_eq!(runtime.pipe(OutputStream::Stdout), Some(combined));
    assert_eq!(runtime.pipe(OutputStream::Stderr), Some(combined));
    assert_eq!(runtime.stages(OutputStream::Stdout).len(), 1);
    assert_eq!(runtime.stages(OutputStream::Stderr).len(), 1);

    runtime.update_stage(
        OutputStream::Stdout,
        0,
        1,
        SupervisorState::Ready(Generation::FIRST),
    )?;
    assert_eq!(
        runtime.health(OutputStream::Stdout),
        Some(PipelineHealth::Ready)
    );
    assert_eq!(
        runtime.health(OutputStream::Stderr),
        Some(PipelineHealth::Ready)
    );
    Ok(())
}

#[test]
fn logger_restart_preserves_pipe_and_reports_health() -> Result<(), Box<dyn Error>> {
    let config = parse_str(
        "version: 2\ncommand: [/bin/true]\nlog:\n  file: /tmp/out.log\nlogger: [/usr/bin/logger, -t, service]\n",
    )?;
    let mut runtime = LoggingRuntime::new(LoggingPlan::from_config(&config.logging)?);
    let pipe = runtime.pipe(OutputStream::Combined).ok_or("pipe missing")?;
    let logger_pipe = runtime.logger_pipe().ok_or("logger pipe missing")?;
    assert_eq!(
        runtime.health(OutputStream::Combined),
        Some(PipelineHealth::Starting)
    );

    runtime.update_stage(
        OutputStream::Combined,
        0,
        1,
        SupervisorState::Ready(Generation::FIRST),
    )?;
    runtime.update_logger(1, SupervisorState::Ready(Generation::FIRST))?;
    assert_eq!(
        runtime.health(OutputStream::Combined),
        Some(PipelineHealth::Ready)
    );
    runtime.update_logger(
        2,
        SupervisorState::Backoff {
            generation: Generation::FIRST,
            delay_seconds: 2,
        },
    )?;
    assert_eq!(runtime.pipe(OutputStream::Combined), Some(pipe));
    assert_eq!(runtime.logger_pipe(), Some(logger_pipe));
    assert_eq!(
        runtime.health(OutputStream::Combined),
        Some(PipelineHealth::Backoff)
    );
    runtime.update_logger(2, SupervisorState::Failed(FailureReason::RetryLimit))?;
    assert_eq!(
        runtime.health(OutputStream::Combined),
        Some(PipelineHealth::Failed)
    );
    Ok(())
}

#[test]
fn shutdown_order_cannot_stop_loggers_before_service_and_drain() -> Result<(), Box<dyn Error>> {
    let mut shutdown = LoggingShutdown::default();
    assert!(shutdown.drain_finished().is_err());
    assert_eq!(shutdown.begin(true)?, LoggingShutdownEffect::StopService);
    assert!(shutdown.loggers_stopped().is_err());
    assert_eq!(
        shutdown.service_stopped()?,
        LoggingShutdownEffect::BeginDrain
    );
    assert_eq!(
        shutdown.drain_finished()?,
        LoggingShutdownEffect::StopLoggersDownstreamFirst
    );
    assert_eq!(shutdown.loggers_stopped()?, LoggingShutdownEffect::Complete);
    Ok(())
}
