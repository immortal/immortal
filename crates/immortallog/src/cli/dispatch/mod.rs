//! Conversion from Clap matches into typed write or inspection actions.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
    time::Duration,
};

use clap::ArgMatches;
use immortal_core::logging::RotationPolicy;

use crate::cli::actions::{Action, ArchivesAction, OutputFormat, WriteAction};

/// Required parser invariant was absent.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchError(&'static str);

impl Display for DispatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for DispatchError {}

/// Convert parsed matches into one typed logger action.
///
/// # Errors
///
/// Returns an error if a required parser invariant is absent.
pub fn action(matches: &ArgMatches) -> Result<Action, DispatchError> {
    if let Some(("archives", matches)) = matches.subcommand() {
        return archives_action(matches).map(Action::Archives);
    }
    write_action(matches).map(Action::Write)
}

fn write_action(matches: &ArgMatches) -> Result<WriteAction, DispatchError> {
    let file = matches
        .get_one::<PathBuf>("file")
        .cloned()
        .ok_or(DispatchError("missing log destination"))?;
    Ok(WriteAction {
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

fn archives_action(matches: &ArgMatches) -> Result<ArchivesAction, DispatchError> {
    let file = matches
        .get_one::<PathBuf>("file")
        .cloned()
        .ok_or(DispatchError("missing archive namespace"))?;
    let output = match matches.get_one::<String>("output").map(String::as_str) {
        Some("table") => OutputFormat::Table,
        Some("json") => OutputFormat::Json,
        Some(_) => return Err(DispatchError("unknown archive output format")),
        None => return Err(DispatchError("missing archive output format")),
    };
    Ok(ArchivesAction { file, output })
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use super::action;
    use crate::cli::{
        actions::{Action, OutputFormat},
        commands,
    };

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
        let Action::Write(action) = action else {
            return Err("expected write action".into());
        };
        assert_eq!(action.file, PathBuf::from("/tmp/api.log"));
        assert_eq!(action.rotation.max_age.map(|age| age.as_secs()), Some(60));
        assert_eq!(action.rotation.max_total_bytes, Some(4096));
        assert!(action.timestamp);
        assert!(action.passthrough);
        Ok(())
    }

    #[test]
    fn captures_archive_namespace_and_output_format() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortallog",
            "archives",
            "-o",
            "json",
            "/tmp/api.log",
        ])?;
        let Action::Archives(action) = action(&matches)? else {
            return Err("expected archives action".into());
        };
        assert_eq!(action.file, PathBuf::from("/tmp/api.log"));
        assert_eq!(action.output, OutputFormat::Json);
        Ok(())
    }
}
