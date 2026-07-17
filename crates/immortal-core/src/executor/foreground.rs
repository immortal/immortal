//! Foreground executor entry points.
//!
//! Foreground startup prepares all commands before the broker is created, then
//! delegates to the shared runtime path. Controlled foreground startup first
//! acquires the runtime-directory owner and keeps that ownership until broker
//! cleanup completes, preserving the authenticated control socket and PID-file
//! safety invariants.

use std::{io, path::Path};

use super::{
    ControlSetup, ExecutorError, RuntimeOwner, ServiceConfig, StartupReporter, SupervisionOutcome,
    prepare_execution, run_prepared,
};

/// Run one service through a broker created before the current-thread Tokio runtime.
///
/// The executor owns service groups, readiness, hooks, logger chains, restart
/// policy, bounded backoff, and complete broker shutdown. Configuration which
/// requires a directory manager or persistent control owner fails closed
/// through [`ExecutorError::Unsupported`].
///
/// # Errors
///
/// Returns configuration-capability, process, broker, or state-transition failures.
pub fn run_foreground(config: &ServiceConfig) -> Result<SupervisionOutcome, ExecutorError> {
    let commands = prepare_execution(config, false)?;
    run_prepared(config, None, commands, &mut StartupReporter::Foreground)
}

/// Run one foreground service while exclusively owning an authenticated control endpoint.
///
/// `directory` is the exact absolute `ROOT/SERVICE` runtime directory. Its
/// parent must already exist and satisfy the runtime-root permission contract.
/// The ownership lock is acquired before the process broker is forked, and is
/// retained until the broker has completed child cleanup.
///
/// # Errors
///
/// Returns configuration, runtime ownership, control-socket, broker, process,
/// or lifecycle failures.
pub fn run_foreground_controlled(
    config: &ServiceConfig,
    directory: &Path,
) -> Result<SupervisionOutcome, ExecutorError> {
    let owner = RuntimeOwner::acquire(directory)?;
    let service_name = owner
        .directory()
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid service name"))?
        .to_owned();
    let commands = prepare_execution(config, true)?;
    run_prepared(
        config,
        Some(ControlSetup {
            owner,
            service_name,
        }),
        commands,
        &mut StartupReporter::Foreground,
    )
}
