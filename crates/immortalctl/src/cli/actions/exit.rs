//! Supervisor-exit operation routing.

use immortal_core::control::Operation;

use super::{ActionError, ControlAction, control};

/// Execute one supervisor-exit request.
///
/// # Errors
///
/// Returns a typed discovery, transport, lifecycle, response, or output failure.
pub fn execute(action: &ControlAction) -> Result<(), ActionError> {
    control::execute(action, Operation::Exit)
}
