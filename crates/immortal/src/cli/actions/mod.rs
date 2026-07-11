//! Coordination of application operations selected by CLI dispatch.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
};

use immortal_core::{
    config::{ConfigError, emit_config, parse_file},
    exit::ExitClass,
};

use crate::cli::dispatch::Action;

/// Failure while coordinating an application action.
#[derive(Debug)]
pub enum ActionError {
    /// Configuration parsing, normalization, or emission failed.
    Config(ConfigError),
    /// Normalized output could not be written.
    Output(io::Error),
    /// Process execution has not yet crossed its contract-test gate.
    SupervisionUnavailable,
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(error) => Display::fmt(error, formatter),
            Self::Output(error) => write!(formatter, "unable to write output: {error}"),
            Self::SupervisionUnavailable => formatter.write_str(
                "process supervision is not enabled yet; use --check-config to validate a service",
            ),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Config(error) => Some(error),
            Self::Output(error) => Some(error),
            Self::SupervisionUnavailable => None,
        }
    }
}

impl From<ConfigError> for ActionError {
    fn from(error: ConfigError) -> Self {
        Self::Config(error)
    }
}

impl From<io::Error> for ActionError {
    fn from(error: io::Error) -> Self {
        Self::Output(error)
    }
}

impl ActionError {
    /// Stable process exit classification for this failure.
    #[must_use]
    pub const fn exit_class(&self) -> ExitClass {
        match self {
            Self::Config(_) => ExitClass::Configuration,
            Self::Output(_) => ExitClass::IoError,
            Self::SupervisionUnavailable => ExitClass::Unavailable,
        }
    }
}

/// Execute one typed action.
///
/// # Errors
///
/// Returns an error when input is invalid, output fails, or a requested
/// operational capability has not yet passed its implementation gate.
pub fn execute(action: Action) -> Result<(), ActionError> {
    match action {
        Action::CheckConfig(path) => {
            let config = parse_file(&path)?;
            let output = emit_config(&config)?;
            io::stdout().lock().write_all(output.as_bytes())?;
            Ok(())
        }
        Action::SuperviseConfig(_) | Action::SuperviseCommand(_) => {
            Err(ActionError::SupervisionUnavailable)
        }
    }
}
