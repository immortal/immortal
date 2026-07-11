//! Conversion from Clap matches into a typed logger action.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
    time::Duration,
};

use clap::ArgMatches;
use immortal_core::logging::RotationPolicy;

/// Fully typed file adapter action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Action {
    /// Destination file.
    pub file: PathBuf,
    /// Rotation and retention limits.
    pub rotation: RotationPolicy,
    /// Prefix logical file records with a timestamp.
    pub timestamp: bool,
    /// Copy original bytes to stdout after durable file writes.
    pub passthrough: bool,
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

/// Convert parsed matches into one typed adapter action.
///
/// # Errors
///
/// Returns an error if a required parser invariant is absent.
pub fn action(matches: &ArgMatches) -> Result<Action, DispatchError> {
    let file = matches
        .get_one::<String>("file")
        .map(PathBuf::from)
        .ok_or(DispatchError("missing log destination"))?;
    Ok(Action {
        file,
        rotation: RotationPolicy {
            max_bytes: matches.get_one::<u64>("max-bytes").copied(),
            max_age: matches
                .get_one::<u64>("max-age")
                .copied()
                .map(Duration::from_secs),
            keep: matches
                .get_one::<u32>("keep")
                .copied()
                .map(usize::try_from)
                .transpose()
                .map_err(|_| DispatchError("archive count does not fit this platform"))?,
            max_total_bytes: matches.get_one::<u64>("max-total-bytes").copied(),
        },
        timestamp: matches.get_flag("timestamp"),
        passthrough: matches.get_flag("passthrough"),
    })
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use super::action;
    use crate::cli::commands;

    #[test]
    fn preserves_rotation_and_stream_flags() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortallog",
            "--max-age",
            "60",
            "--max-total-bytes",
            "4096",
            "--timestamp",
            "--passthrough",
            "/tmp/api.log",
        ])?;
        let action = action(&matches)?;
        assert_eq!(action.file, PathBuf::from("/tmp/api.log"));
        assert_eq!(action.rotation.max_age.map(|age| age.as_secs()), Some(60));
        assert_eq!(action.rotation.max_total_bytes, Some(4096));
        assert!(action.timestamp);
        assert!(action.passthrough);
        Ok(())
    }
}
