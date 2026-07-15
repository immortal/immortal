//! Validation and canonical emission for one configuration file.
//!
//! The handler parses through the owning core boundary before writing anything,
//! then emits only the canonical strict schema. Invalid input and output
//! failures remain distinguishable through the shared typed action error.

use std::{
    io::{self, Write},
    path::Path,
};

use immortal_core::config::{emit_config, parse_file};

use super::ActionError;

/// Validate one definition and write its canonical representation to stdout.
///
/// # Errors
///
/// Returns an error when parsing, normalization, or output fails.
pub fn execute(path: &Path) -> Result<(), ActionError> {
    let config = parse_file(path)?;
    let output = emit_config(&config)?;
    io::stdout().lock().write_all(output.as_bytes())?;
    Ok(())
}
