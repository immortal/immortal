//! Conversion from Clap matches into typed application actions.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::PathBuf,
};

use clap::ArgMatches;

use crate::cli::actions::{Action, DirectService, RuntimeIdentity};

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
            Ok(Action::SuperviseConfig {
                control_directory: matches.get_one::<String>("control-dir").map(PathBuf::from),
                path,
                foreground: matches.get_flag("foreground"),
            })
        };
    }

    let command = matches
        .get_many::<String>("command")
        .ok_or(DispatchError("missing service command"))?
        .cloned()
        .collect();
    let runtime_identity = if let Some(directory) = matches.get_one::<String>("control-dir") {
        RuntimeIdentity::ControlDirectory(PathBuf::from(directory))
    } else {
        RuntimeIdentity::Name(
            matches
                .get_one::<String>("name")
                .ok_or(DispatchError("missing direct-command service name"))?
                .clone(),
        )
    };
    Ok(Action::SuperviseCommand(DirectService {
        child_pid: matches.get_one::<String>("child-pid").map(PathBuf::from),
        command,
        environment_directory: matches.get_one::<String>("env-dir").map(PathBuf::from),
        foreground: matches.get_flag("foreground"),
        logfile: matches.get_one::<String>("logfile").map(PathBuf::from),
        logger: matches
            .get_many::<String>("logger")
            .map(|values| values.cloned().collect()),
        retries: matches.get_one::<i32>("retries").copied().unwrap_or(-1),
        runtime_identity,
        start_delay_seconds: matches.get_one::<u64>("wait").copied().unwrap_or(0),
        supervisor_pid: matches
            .get_one::<String>("supervisor-pid")
            .map(PathBuf::from),
        user: matches.get_one::<String>("user").cloned(),
        working_directory: matches.get_one::<String>("working-dir").map(PathBuf::from),
    }))
}

#[cfg(test)]
mod tests {
    use std::{error::Error, path::PathBuf};

    use super::action;
    use crate::cli::{
        actions::{Action, DirectService, RuntimeIdentity},
        commands,
    };

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
            "--name",
            "shell",
            "/bin/sh",
            "-c",
            "echo ready",
            "--child-option",
        ])?;
        assert_eq!(
            action(&matches)?,
            Action::SuperviseCommand(DirectService {
                child_pid: None,
                command: vec![
                    "/bin/sh".to_owned(),
                    "-c".to_owned(),
                    "echo ready".to_owned(),
                    "--child-option".to_owned(),
                ],
                environment_directory: None,
                foreground: false,
                logfile: None,
                logger: None,
                retries: -1,
                runtime_identity: RuntimeIdentity::Name("shell".to_owned()),
                start_delay_seconds: 0,
                supervisor_pid: None,
                user: None,
                working_directory: None,
            })
        );
        Ok(())
    }

    #[test]
    fn captures_supported_direct_options() -> Result<(), Box<dyn Error>> {
        let matches = commands::new().try_get_matches_from([
            "immortal",
            "--foreground",
            "--name",
            "true-test",
            "--retries",
            "3",
            "--wait",
            "2",
            "--working-dir",
            "/tmp",
            "--child-pid",
            "/tmp/child.pid",
            "--supervisor-pid",
            "/tmp/supervisor.pid",
            "--env-dir",
            "/tmp/environment",
            "--logfile",
            "/tmp/service.log",
            "--user",
            "service-account",
            "--logger",
            "/usr/bin/logger",
            "-t",
            "service",
            "--",
            "/bin/true",
        ])?;
        assert_eq!(
            action(&matches)?,
            Action::SuperviseCommand(DirectService {
                child_pid: Some(PathBuf::from("/tmp/child.pid")),
                command: vec!["/bin/true".to_owned()],
                environment_directory: Some(PathBuf::from("/tmp/environment")),
                foreground: true,
                logfile: Some(PathBuf::from("/tmp/service.log")),
                logger: Some(vec![
                    "/usr/bin/logger".to_owned(),
                    "-t".to_owned(),
                    "service".to_owned(),
                ]),
                retries: 3,
                runtime_identity: RuntimeIdentity::Name("true-test".to_owned()),
                start_delay_seconds: 2,
                supervisor_pid: Some(PathBuf::from("/tmp/supervisor.pid")),
                user: Some("service-account".to_owned()),
                working_directory: Some(PathBuf::from("/tmp")),
            })
        );
        Ok(())
    }

    #[test]
    fn carries_an_exact_control_directory_for_commands_and_configs() -> Result<(), Box<dyn Error>> {
        let command = commands::new().try_get_matches_from([
            "immortal",
            "--control-dir",
            "/run/immortal/api",
            "/bin/true",
        ])?;
        let Action::SuperviseCommand(command) = action(&command)? else {
            return Err("expected a direct command action".into());
        };
        assert_eq!(
            command.runtime_identity,
            RuntimeIdentity::ControlDirectory(PathBuf::from("/run/immortal/api"))
        );
        let config = commands::new().try_get_matches_from([
            "immortal",
            "--config",
            "run.yml",
            "--control-dir",
            "/run/immortal/api",
        ])?;
        assert_eq!(
            action(&config)?,
            Action::SuperviseConfig {
                control_directory: Some(PathBuf::from("/run/immortal/api")),
                path: PathBuf::from("run.yml"),
                foreground: false,
            }
        );
        Ok(())
    }
}
