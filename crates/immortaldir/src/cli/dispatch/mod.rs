//! Conversion from Clap matches into typed reconciliation actions.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
};

use clap::ArgMatches;
use immortal_core::reconcile::LaunchConcurrency;

use crate::cli::actions::{Action, ReconcileAction};

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
    let supervisor_binary = matches
        .get_one::<String>("supervisor-binary")
        .map(PathBuf::from)
        .ok_or(DispatchError("missing supervisor binary"))?;
    let launch_concurrency = match matches.get_one::<u64>("max-concurrent-starts").copied() {
        Some(value) => usize::try_from(value)
            .map_err(|_| DispatchError("invalid maximum concurrent starts"))
            .and_then(|value| {
                LaunchConcurrency::new(value)
                    .map_err(|_| DispatchError("invalid maximum concurrent starts"))
            })?,
        None => LaunchConcurrency::default(),
    };
    Ok(Action::Reconcile(ReconcileAction {
        directory,
        runtime_directory,
        scan_interval_seconds,
        supervisor_binary,
        launch_concurrency,
        once: matches.get_flag("once"),
        dry_run: matches.get_flag("dry-run"),
    }))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use immortal_core::reconcile::LaunchConcurrency;

    use super::action;
    use crate::cli::{
        actions::{Action, ReconcileAction},
        commands,
    };

    #[test]
    fn preserves_dry_run_inputs() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortaldir",
            "--runtime-dir",
            "/tmp/runtime",
            "--scan-interval",
            "30",
            "--max-concurrent-starts",
            "4",
            "--once",
            "--dry-run",
            "/tmp/services",
        ])?;
        assert_eq!(
            action(&matches)?,
            Action::Reconcile(ReconcileAction {
                directory: PathBuf::from("/tmp/services"),
                runtime_directory: PathBuf::from("/tmp/runtime"),
                scan_interval_seconds: 30,
                supervisor_binary: PathBuf::from("immortal"),
                launch_concurrency: LaunchConcurrency::new(4)?,
                once: true,
                dry_run: true,
            })
        );
        Ok(())
    }
}
