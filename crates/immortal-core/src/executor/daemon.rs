//! Checked daemon executor entry point.
//!
//! `run_daemon` performs only preparation before entering the checked daemonize
//! sequence. The parent returns after the startup handshake; the detached child
//! acquires any requested control ownership, starts the broker and runtime, and
//! reports initialization failure through the daemon startup channel before
//! exiting. Tokio is not created until after fork and descriptor setup finish.

use std::path::Path;

use super::{
    ControlSetup, DAEMON_STARTUP_TIMEOUT, DaemonRunOutcome, Daemonized, ExecutorError,
    ServiceConfig, StartupReporter, daemonize, prepare_execution, run_prepared,
};

/// Detach one fully materialized service before creating Tokio or the broker.
///
/// The original invoker returns only after the detached child has acquired any
/// configured runtime ownership, started the broker and runtime, and bound the
/// authenticated control listener. The detached child continues supervision.
///
/// # Errors
///
/// Returns preparation or checked daemon errors to the original invoker. The
/// detached child reports initialization failures through the startup channel.
pub fn run_daemon(
    config: &ServiceConfig,
    control_directory: Option<&Path>,
) -> Result<DaemonRunOutcome, ExecutorError> {
    let commands = prepare_execution(config, control_directory.is_some())?;
    match daemonize(DAEMON_STARTUP_TIMEOUT)? {
        Daemonized::Parent { .. } => Ok(DaemonRunOutcome::Parent),
        Daemonized::Daemon(notifier) => {
            let mut startup = StartupReporter::Daemon(Some(notifier));
            let execution = (|| {
                let control = control_directory.map(ControlSetup::acquire).transpose()?;
                run_prepared(config, control, commands, &mut startup)
            })();
            if let Err(error) = &execution {
                startup.fail_if_pending(error);
            }
            execution.map(DaemonRunOutcome::Daemon)
        }
    }
}
