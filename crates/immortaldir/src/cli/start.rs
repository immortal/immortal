use std::{
    io::{self, Write},
    process::ExitCode,
};

use tracing_subscriber::EnvFilter;

use immortal_core::exit::ExitClass;

use crate::cli::{actions, commands, dispatch};

/// Parse the command line, initialize diagnostics, and execute reconciliation.
#[must_use]
pub fn start() -> ExitCode {
    let matches = commands::new().get_matches();
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
            let endpoint = if action.dry_run {
                None
            } else {
                Some(
                    immortal_core::process::start_process_broker()
                        .map_err(|error| (ExitClass::OsError, error.to_string()))?,
                )
            };
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| (ExitClass::Software, error.to_string()))?
                .block_on(actions::execute(&action, endpoint))
                .map_err(|error| (error.exit_class(), error.to_string()))
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err((class, error)) => {
            let _ignored = writeln!(io::stderr().lock(), "immortaldir: {error}");
            class.exit_code()
        }
    }
}
