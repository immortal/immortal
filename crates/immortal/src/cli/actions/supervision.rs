//! Shared foreground and daemon supervision completion.
//!
//! The flow enters the core through either controlled foreground execution or
//! checked daemonization, then classifies the final lifecycle outcome. The
//! parent side of a successful daemon handoff returns immediately; the
//! supervising side preserves terminal startup and readiness failures.

use std::path::Path;

use immortal_core::{
    config::ServiceConfig,
    executor::{DaemonRunOutcome, SupervisionOutcome, run_daemon, run_foreground_controlled},
    supervisor::SupervisorState,
};

use super::ActionError;

pub(super) fn execute(
    config: &ServiceConfig,
    control_directory: &Path,
    foreground: bool,
) -> Result<(), ActionError> {
    if foreground {
        let outcome = run_foreground_controlled(config, control_directory)?;
        finish(outcome)
    } else {
        match run_daemon(config, Some(control_directory))? {
            DaemonRunOutcome::Parent => Ok(()),
            DaemonRunOutcome::Daemon(outcome) => finish(outcome),
        }
    }
}

fn finish(outcome: SupervisionOutcome) -> Result<(), ActionError> {
    if outcome.last_start_failed
        || outcome.last_readiness_failed
        || outcome.terminal_failure.is_some()
        || matches!(outcome.state, SupervisorState::Failed(_))
    {
        Err(ActionError::ServiceFailed(outcome))
    } else {
        Ok(())
    }
}
