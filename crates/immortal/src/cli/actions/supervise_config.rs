//! Supervision setup for one strict configuration file.
//!
//! The definition is fully parsed before runtime ownership is acquired. An
//! explicit control directory is preserved exactly; otherwise the UTF-8 file
//! stem becomes the service identity under the effective user's runtime root.

use std::{
    io,
    path::{Path, PathBuf},
};

use immortal_core::{config::parse_file, runtime::prepare_user_service_directory};

use super::{ActionError, supervision};

/// Load one definition, resolve its runtime identity, and supervise it.
///
/// # Errors
///
/// Returns an error when configuration parsing, runtime preparation, or
/// supervision fails.
pub fn execute(
    path: &Path,
    control_directory: Option<PathBuf>,
    foreground: bool,
) -> Result<(), ActionError> {
    let config = parse_file(path)?;
    let control_directory = match control_directory {
        Some(directory) => directory,
        None => runtime_directory(path)?,
    };
    supervision::execute(&config, &control_directory, foreground)
}

fn runtime_directory(path: &Path) -> Result<PathBuf, ActionError> {
    let name = path
        .file_stem()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            ActionError::Runtime(io::Error::new(
                io::ErrorKind::InvalidInput,
                "configuration filename has no UTF-8 service stem",
            ))
        })?;
    prepare_user_service_directory(name).map_err(ActionError::Runtime)
}
