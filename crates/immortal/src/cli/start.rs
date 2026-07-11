use std::{
    io::{self, Write},
    process::ExitCode,
};

use tracing_subscriber::EnvFilter;

use immortal_core::exit::ExitClass;

use crate::cli::{actions, commands, dispatch};

/// Parse the command line, initialize diagnostics, and execute the selected action.
#[must_use]
pub fn start() -> ExitCode {
    let matches =
        commands::try_get_matches_from(std::env::args_os()).unwrap_or_else(|error| error.exit());
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"));

    drop(
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(io::stderr)
            .try_init(),
    );

    let result = dispatch::action(&matches)
        .map_err(|error| (ExitClass::Software, error.to_string()))
        .and_then(|action| {
            actions::execute(action).map_err(|error| (error.exit_class(), error.to_string()))
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err((class, error)) => {
            let _ignored = writeln!(io::stderr().lock(), "immortal: {error}");
            class.exit_code()
        }
    }
}
