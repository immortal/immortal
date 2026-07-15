//! CLI parsing, diagnostic initialization, and stable process completion.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
    process::ExitCode,
};

use clap::Error as ClapError;
use tracing_subscriber::EnvFilter;

use immortal_core::exit::ExitClass;

use crate::cli::{
    actions::{Action, ActionError},
    commands,
    dispatch::{self, DispatchError},
};

/// Failure before a typed control action can be returned.
#[derive(Debug)]
pub enum StartError {
    /// Clap rejected the argument vector or rendered help/version output.
    Arguments(ClapError),
    /// Parsed matches violated an internal dispatch invariant.
    Dispatch(DispatchError),
}

impl Display for StartError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Arguments(error) => Display::fmt(error, formatter),
            Self::Dispatch(error) => Display::fmt(error, formatter),
        }
    }
}

impl Error for StartError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Arguments(error) => Some(error),
            Self::Dispatch(error) => Some(error),
        }
    }
}

impl StartError {
    /// Render the startup failure and return its stable process status.
    #[must_use]
    pub fn report(self) -> ExitCode {
        match self {
            Self::Arguments(error) => {
                let class = if error.use_stderr() {
                    ExitClass::Usage
                } else {
                    ExitClass::Success
                };
                exit_after_output(&error.print(), class)
            }
            Self::Dispatch(error) => exit_after_output(
                &writeln!(io::stderr().lock(), "immortalctl: {error}"),
                ExitClass::Software,
            ),
        }
    }
}

/// Parse the command line, initialize diagnostics, and return one typed action.
///
/// # Errors
///
/// Returns a structured parser or dispatch failure.
pub fn start() -> Result<Action, StartError> {
    let matches =
        commands::try_get_matches_from(std::env::args_os()).map_err(StartError::Arguments)?;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    drop(
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .try_init(),
    );

    dispatch::action(&matches).map_err(StartError::Dispatch)
}

/// Render an action result and return its stable process status.
#[must_use]
pub fn finish(result: Result<(), ActionError>) -> ExitCode {
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => exit_after_output(
            &writeln!(io::stderr().lock(), "immortalctl: {error}"),
            error.exit_class(),
        ),
    }
}

fn exit_after_output(output: &io::Result<()>, class: ExitClass) -> ExitCode {
    match output {
        Ok(()) => class.exit_code(),
        Err(_) => ExitClass::IoError.exit_code(),
    }
}
