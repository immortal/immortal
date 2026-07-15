//! Status operation routing.

use immortal_core::control::Operation;

use super::{ActionError, ControlAction, control};

/// Execute one status request.
///
/// # Errors
///
/// Returns a typed discovery, transport, response, or output failure.
pub fn execute(action: &ControlAction) -> Result<(), ActionError> {
    control::execute(action, Operation::Status)
}
