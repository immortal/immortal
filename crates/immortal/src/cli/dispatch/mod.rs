//! Conversion from Clap matches into typed application actions.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
};

use clap::ArgMatches;

/// Typed operation selected by the command line.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Validate a file and emit the normalized supported schema.
    CheckConfig(PathBuf),
    /// Supervise the service described by a configuration file.
    SuperviseConfig(PathBuf),
    /// Supervise a direct argv command.
    SuperviseCommand(Vec<String>),
}

/// A required invariant was absent from otherwise valid Clap matches.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DispatchError(&'static str);

impl Display for DispatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.0)
    }
}

impl Error for DispatchError {}

/// Convert parsed command-line matches into a typed action.
///
/// # Errors
///
/// Returns an error if a parser invariant is violated.
pub fn action(matches: &ArgMatches) -> Result<Action, DispatchError> {
    if let Some(path) = matches.get_one::<String>("config") {
        let path = PathBuf::from(path);
        return if matches.get_flag("check-config") {
            Ok(Action::CheckConfig(path))
        } else {
            Ok(Action::SuperviseConfig(path))
        };
    }

    let command = matches
        .get_many::<String>("command")
        .ok_or(DispatchError("missing service command"))?
        .cloned()
        .collect();
    Ok(Action::SuperviseCommand(command))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use super::{Action, action};
    use crate::cli::commands;

    #[test]
    fn selects_config_check() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortal",
            "--config",
            "run.yml",
            "--check-config",
        ])?;
        assert_eq!(
            action(&matches)?,
            Action::CheckConfig(PathBuf::from("run.yml"))
        );
        Ok(())
    }

    #[test]
    fn preserves_direct_argv() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortal",
            "/bin/sh",
            "-c",
            "echo ready",
            "--child-option",
        ])?;
        assert_eq!(
            action(&matches)?,
            Action::SuperviseCommand(vec![
                "/bin/sh".to_owned(),
                "-c".to_owned(),
                "echo ready".to_owned(),
                "--child-option".to_owned(),
            ])
        );
        Ok(())
    }
}
