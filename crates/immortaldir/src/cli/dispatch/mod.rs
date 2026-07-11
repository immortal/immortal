//! Conversion from Clap matches into typed reconciliation actions.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
};

use clap::ArgMatches;

/// Typed `immortaldir` operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Action {
    /// Definitions directory.
    pub directory: PathBuf,
    /// Runtime state root reserved for operational reconciliation.
    pub runtime_directory: PathBuf,
    /// Safety reconciliation interval.
    pub scan_interval_seconds: u64,
    /// Exit after one complete reconciliation.
    pub once: bool,
    /// Print the desired plan without mutations.
    pub dry_run: bool,
}

/// Required parser invariant was absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchError(&'static str);

impl Display for DispatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for DispatchError {}

/// Convert parsed matches into a typed reconciliation action.
///
/// # Errors
///
/// Returns an error if a required parser invariant is absent.
pub fn action(matches: &ArgMatches) -> Result<Action, DispatchError> {
    let directory = matches
        .get_one::<String>("directory")
        .map(PathBuf::from)
        .ok_or(DispatchError("missing definitions directory"))?;
    let runtime_directory = matches
        .get_one::<String>("runtime-dir")
        .map(PathBuf::from)
        .ok_or(DispatchError("missing runtime directory"))?;
    let scan_interval_seconds = matches
        .get_one::<u64>("scan-interval")
        .copied()
        .ok_or(DispatchError("missing scan interval"))?;
    Ok(Action {
        directory,
        runtime_directory,
        scan_interval_seconds,
        once: matches.get_flag("once"),
        dry_run: matches.get_flag("dry-run"),
    })
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use super::{Action, action};
    use crate::cli::commands;

    #[test]
    fn preserves_dry_run_inputs() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortaldir",
            "--runtime-dir",
            "/tmp/runtime",
            "--scan-interval",
            "30",
            "--once",
            "--dry-run",
            "/tmp/services",
        ])?;
        assert_eq!(
            action(&matches)?,
            Action {
                directory: PathBuf::from("/tmp/services"),
                runtime_directory: PathBuf::from("/tmp/runtime"),
                scan_interval_seconds: 30,
                once: true,
                dry_run: true,
            }
        );
        Ok(())
    }
}
