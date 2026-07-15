//! Run-once operation routing.

use immortal_core::control::Operation;

use super::{ActionError, ControlAction, control};

/// Execute one run-once request.
///
/// # Errors
///
/// Returns a typed discovery, transport, lifecycle, response, or output failure.
pub fn execute(action: &ControlAction) -> Result<(), ActionError> {
    control::execute(action, Operation::Once)
}
