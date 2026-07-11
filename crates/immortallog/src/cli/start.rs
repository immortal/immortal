use std::{
    io::{self, Write},
    process::ExitCode,
};

use immortal_core::exit::ExitClass;

use crate::cli::{actions, commands, dispatch};

/// Parse and run the logging adapter.
#[must_use]
pub fn start() -> ExitCode {
    let matches = commands::new().get_matches();
    let result = dispatch::action(&matches)
        .map_err(|error| (ExitClass::Software, error.to_string()))
        .and_then(|action| {
            actions::execute(&action).map_err(|error| (error.exit_class(), error.to_string()))
        });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err((class, error)) => {
            let _ignored = writeln!(io::stderr().lock(), "immortallog: {error}");
            class.exit_code()
        }
    }
}
