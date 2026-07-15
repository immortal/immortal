//! Restart operation routing.

use immortal_core::control::Operation;

use super::{ActionError, ControlAction, control};

/// Execute one restart request.
///
/// # Errors
///
/// Returns a typed discovery, transport, lifecycle, response, or output failure.
pub fn execute(action: &ControlAction) -> Result<(), ActionError> {
    control::execute(action, Operation::Restart)
}
