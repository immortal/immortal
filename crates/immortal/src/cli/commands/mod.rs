//! Clap command and option definitions for `immortal`.

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
};

use clap::{
    Arg, ArgAction, ArgGroup, ArgMatches, ColorChoice, Command, Error, ValueHint,
    builder::styling::{AnsiColor, Effects, Styles},
    error::ErrorKind,
};
use immortal_core::config::MAX_SCHEDULE_SECONDS;

const CONFIG_CONFLICTS: [&str; 10] = [
    "env-dir",
    "logfile",
    "logger",
    "name",
    "retries",
    "child-pid",
    "supervisor-pid",
    "user",
    "working-dir",
    "wait",
];

/// Build the command-line interface.
#[must_use]
pub fn new() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(immortal_core::build_info::long_version())
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .long_about(
            "Run a command detached from its controlling terminal, supervise it, and restart it \
             when it exits. Foreground, checked daemon startup, and authenticated control are \
             backed by process contracts.",
        )
        .override_usage(
            "immortal [OPTIONS] (-n <SERVICE> | --control-dir <DIR>) <COMMAND> [ARGUMENTS]...\n       \
             immortal [OPTIONS] --config <FILE>",
        )
        .after_help(
            "Examples:
  immortal --foreground --name probe --retries 0 /bin/true
  immortal --name api --logfile /var/log/api.log /usr/local/bin/api
  immortal --name api --logger /usr/bin/logger -t api -- /usr/local/bin/api
  immortal --foreground --config /usr/local/etc/immortal/run.yml
  immortal --config run.yml --check-config",
        )
        .color(ColorChoice::Auto)
        .styles(styles())
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .trailing_var_arg(true)
        .group(
            ArgGroup::new("service-source")
                .args(["config", "command"])
                .required(true),
        )
        .arg(arg_foreground())
        .arg(arg_name())
        .arg(arg_retries())
        .arg(arg_check_config())
        .arg(arg_child_pid())
        .arg(arg_config())
        .arg(arg_control_dir())
        .arg(arg_environment_directory())
        .arg(arg_logfile())
        .arg(arg_logger())
        .arg(arg_supervisor_pid())
        .arg(arg_user())
        .arg(arg_working_dir())
        .arg(arg_wait())
        .arg(arg_command())
}

/// Parse arguments while rejecting ambiguous historical single-dash spellings.
///
/// Exact logger and child argv remain under those programs' ownership, even
/// when an argument is exactly `-name` or `-logger`.
///
/// # Errors
///
/// Returns Clap's structured syntax or validation error.
pub fn try_get_matches_from<I, T>(arguments: I) -> Result<ArgMatches, Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    let matches = new().try_get_matches_from(arguments.clone())?;
    let command_arguments = matches
        .get_many::<String>("command")
        .map_or(0, Iterator::count);
    let option_end = arguments.len().saturating_sub(command_arguments);
    let logger_indices: BTreeSet<usize> =
        matches.indices_of("logger").into_iter().flatten().collect();
    let ambiguous = arguments
        .iter()
        .enumerate()
        .take(option_end)
        .skip(1)
        .find_map(|(index, argument)| {
            (!logger_indices.contains(&index)
                && (argument == OsStr::new("-name") || argument == OsStr::new("-logger")))
            .then_some(argument)
        });
    if let Some(argument) = ambiguous {
        let message = if argument == OsStr::new("-name") {
            "the historical `-name` spelling is unsupported; use `-n SERVICE` or `--name SERVICE`"
        } else {
            "the historical `-logger` spelling is unsupported; use `--logger PROGRAM [ARGUMENTS]... -- COMMAND"
        };
        return Err(new().error(ErrorKind::UnknownArgument, message));
    }
    Ok(matches)
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}

fn arg_foreground() -> Arg {
    Arg::new("foreground")
        .short('f')
        .long("foreground")
        .help("Stay in the foreground instead of daemonizing")
        .action(ArgAction::SetTrue)
}

fn arg_name() -> Arg {
    Arg::new("name")
        .short('n')
        .long("name")
        .value_name("SERVICE")
        .help("Use SERVICE below the effective user's .immortal runtime root")
        .required_unless_present_any(["config", "control-dir"])
        .conflicts_with_all(["config", "control-dir"])
}

fn arg_retries() -> Arg {
    Arg::new("retries")
        .short('r')
        .long("retries")
        .value_name("COUNT")
        .help("Retries before supervisor exit; -1 means never exit")
        .default_value("-1")
        .allow_hyphen_values(true)
        .value_parser(clap::value_parser!(i32).range(-1..))
        .conflicts_with("config")
}

fn arg_check_config() -> Arg {
    Arg::new("check-config")
        .long("check-config")
        .help("Validate and print the configuration, then exit")
        .action(ArgAction::SetTrue)
        .requires("config")
}

fn arg_child_pid() -> Arg {
    Arg::new("child-pid")
        .short('p')
        .long("child-pid")
        .value_name("PIDFILE")
        .value_hint(ValueHint::FilePath)
        .help("Write the supervised child PID to PIDFILE")
        .conflicts_with("config")
}

fn arg_config() -> Arg {
    Arg::new("config")
        .short('c')
        .long("config")
        .value_name("FILE")
        .value_hint(ValueHint::FilePath)
        .help("Load service configuration from FILE")
        .conflicts_with_all(CONFIG_CONFLICTS)
}

fn arg_control_dir() -> Arg {
    Arg::new("control-dir")
        .long("control-dir")
        .value_name("DIR")
        .value_hint(ValueHint::DirPath)
        .help("Own DIR as this service's runtime directory and serve its control socket")
}

fn arg_environment_directory() -> Arg {
    Arg::new("env-dir")
        .short('e')
        .long("env-dir")
        .value_name("DIR")
        .value_hint(ValueHint::DirPath)
        .help("Set environment variables specified by files in the dir")
        .conflicts_with("config")
}

fn arg_logfile() -> Arg {
    Arg::new("logfile")
        .short('l')
        .long("logfile")
        .value_name("FILE")
        .value_hint(ValueHint::FilePath)
        .help("Write combined stdout/stderr to FILE with 1 MiB rotation and 7 archives")
        .conflicts_with("config")
}

fn arg_logger() -> Arg {
    Arg::new("logger")
        .long("logger")
        .value_name("PROGRAM [ARGUMENTS]...")
        .help("Pipe combined stdout/stderr to exact logger argv; terminate it with --")
        .num_args(1..)
        .allow_hyphen_values(true)
        .value_terminator("--")
        .conflicts_with("config")
}

fn arg_supervisor_pid() -> Arg {
    Arg::new("supervisor-pid")
        .short('P')
        .long("supervisor-pid")
        .value_name("PIDFILE")
        .value_hint(ValueHint::FilePath)
        .help("Write the supervisor PID to PIDFILE")
        .conflicts_with("config")
}

fn arg_user() -> Arg {
    Arg::new("user")
        .short('u')
        .long("user")
        .value_name("USER")
        .value_hint(ValueHint::Username)
        .help("Execute the supervised command as USER")
        .conflicts_with("config")
}

fn arg_working_dir() -> Arg {
    Arg::new("working-dir")
        .short('d')
        .long("working-dir")
        .value_name("DIR")
        .value_hint(ValueHint::DirPath)
        .help("Change to DIR before starting the command")
        .conflicts_with("config")
}

fn arg_wait() -> Arg {
    Arg::new("wait")
        .short('w')
        .long("wait")
        .value_name("SECONDS")
        .help("Wait SECONDS before starting the command")
        .value_parser(clap::value_parser!(u64).range(0..=MAX_SCHEDULE_SECONDS))
        .conflicts_with("config")
}

fn arg_command() -> Arg {
    Arg::new("command")
        .value_name("COMMAND")
        .value_hint(ValueHint::CommandWithArguments)
        .help("Command and arguments to supervise")
        .num_args(1..)
}

#[cfg(test)]
mod tests {
    use clap::{
        builder::styling::{AnsiColor, Effects},
        error::ErrorKind,
    };

    use super::{new, try_get_matches_from};

    #[test]
    fn command_definition_is_valid() {
        new().debug_assert();
    }

    #[test]
    fn help_uses_project_palette() {
        let command = new();
        let styles = command.get_styles();

        assert_eq!(
            styles.get_header(),
            &(AnsiColor::Yellow.on_default() | Effects::BOLD)
        );
        assert_eq!(
            styles.get_usage(),
            &(AnsiColor::Green.on_default() | Effects::BOLD)
        );
        assert_eq!(
            styles.get_literal(),
            &(AnsiColor::Blue.on_default() | Effects::BOLD)
        );
        assert_eq!(styles.get_placeholder(), &AnsiColor::Green.on_default());
    }

    #[test]
    fn env_dir_preserves_released_help_text() {
        let help = new().render_long_help().to_string();
        assert!(help.contains("Set environment variables specified by files in the dir"));
    }

    #[test]
    fn help_is_available() {
        let result = new().try_get_matches_from(["immortal", "--help"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayHelp)
        );
    }

    #[test]
    fn version_is_available() {
        let short = new()
            .try_get_matches_from(["immortal", "-V"])
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            short,
            Some(format!(
                "{} {}\n",
                env!("CARGO_PKG_NAME"),
                env!("CARGO_PKG_VERSION")
            ))
        );

        let long = new()
            .try_get_matches_from(["immortal", "--version"])
            .err()
            .map(|error| error.to_string());
        assert_eq!(
            long,
            Some(format!(
                "{} {}\n",
                env!("CARGO_PKG_NAME"),
                immortal_core::build_info::long_version()
            ))
        );
        assert_eq!(
            new().get_long_version(),
            Some(immortal_core::build_info::long_version())
        );
        for option in ["-V", "--version"] {
            let result = new().try_get_matches_from(["immortal", option]);
            assert_eq!(
                result.err().map(|error| error.kind()),
                Some(ErrorKind::DisplayVersion)
            );
        }
    }

    #[test]
    fn useful_short_options_are_accepted() {
        let result = new().try_get_matches_from([
            "immortal",
            "-f",
            "-n",
            "sleeper",
            "-r",
            "2",
            "-p",
            "/tmp/child.pid",
            "-P",
            "/tmp/supervisor.pid",
            "-u",
            "www",
            "-d",
            "/tmp",
            "-e",
            "/tmp/env",
            "-l",
            "/tmp/sleep.log",
            "-w",
            "3",
            "sleep",
            "30",
        ]);
        assert!(result.is_ok());
    }

    #[test]
    fn modern_long_options_are_typed() {
        let result = new().try_get_matches_from([
            "immortal",
            "--foreground",
            "--retries",
            "2",
            "--control-dir",
            "/tmp/service",
            "--env-dir",
            "/tmp/env",
            "--logfile",
            "/tmp/sleep.log",
            "--wait",
            "3",
            "sleep",
            "30",
        ]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };

        assert!(matches.get_flag("foreground"));
        assert_eq!(matches.get_one::<i32>("retries"), Some(&2));
        assert_eq!(matches.get_one::<u64>("wait"), Some(&3));
        assert_eq!(
            matches.get_one::<String>("control-dir").map(String::as_str),
            Some("/tmp/service")
        );
        assert_eq!(
            matches.get_one::<String>("env-dir").map(String::as_str),
            Some("/tmp/env")
        );
        assert_eq!(
            matches.get_one::<String>("logfile").map(String::as_str),
            Some("/tmp/sleep.log")
        );
    }

    #[test]
    fn config_check_requires_config() {
        let valid = new().try_get_matches_from(["immortal", "-c", "run.yml", "--check-config"]);
        assert!(valid.is_ok());

        let invalid = new().try_get_matches_from(["immortal", "--check-config"]);
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument)
        );
    }

    #[test]
    fn removed_nonoperational_options_are_rejected() {
        for option in ["--follow-pid", "--log-file"] {
            let result = new().try_get_matches_from(["immortal", option, "value", "/bin/true"]);
            assert_eq!(
                result.err().map(|error| error.kind()),
                Some(ErrorKind::UnknownArgument)
            );
        }
    }

    #[test]
    fn exact_logger_argv_and_child_argv_are_separated_by_double_dash() {
        let result = try_get_matches_from([
            "immortal",
            "--name",
            "api",
            "--logfile",
            "/tmp/api.log",
            "--logger",
            "/usr/bin/logger",
            "-t",
            "api",
            "-name",
            "logger-value",
            "--",
            "/usr/local/bin/api",
            "--serve",
        ]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };
        let logger = matches
            .get_many::<String>("logger")
            .map(|values| values.map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        let command = matches
            .get_many::<String>("command")
            .map(|values| values.map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(
            logger,
            ["/usr/bin/logger", "-t", "api", "-name", "logger-value"]
        );
        assert_eq!(command, ["/usr/local/bin/api", "--serve"]);
    }

    #[test]
    fn logger_requires_a_service_separator() {
        let result = try_get_matches_from([
            "immortal",
            "--name",
            "api",
            "--logger",
            "/bin/cat",
            "/usr/local/bin/api",
        ]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument)
        );
    }

    #[test]
    fn historical_spellings_are_rejected_before_but_preserved_after_boundaries() {
        let rejected = try_get_matches_from(["immortal", "-name", "sleep", "sleep", "30"]);
        assert_eq!(
            rejected.err().map(|error| error.kind()),
            Some(ErrorKind::UnknownArgument)
        );
        let rejected_logger =
            try_get_matches_from(["immortal", "--name", "sleep", "-logger", "value", "sleep"]);
        assert_eq!(
            rejected_logger.err().map(|error| error.kind()),
            Some(ErrorKind::UnknownArgument)
        );

        let accepted =
            try_get_matches_from(["immortal", "--name", "shell", "sh", "-name", "child-value"]);
        assert!(accepted.is_ok());
        let command = accepted
            .ok()
            .and_then(|matches| {
                matches
                    .get_many::<String>("command")
                    .map(|values| values.cloned().collect::<Vec<_>>())
            })
            .unwrap_or_default();
        assert_eq!(command, ["sh", "-name", "child-value"]);
    }

    #[test]
    fn config_rejects_ignored_command_options() {
        for (option, value) in [
            ("--env-dir", "/tmp/env"),
            ("--logfile", "/tmp/api.log"),
            ("--name", "api"),
            ("--retries", "2"),
            ("--child-pid", "/tmp/child.pid"),
            ("--supervisor-pid", "/tmp/supervisor.pid"),
            ("--user", "www"),
            ("--working-dir", "/tmp"),
            ("--wait", "2"),
        ] {
            let result =
                new().try_get_matches_from(["immortal", "--config", "run.yml", option, value]);
            assert_eq!(
                result.err().map(|error| error.kind()),
                Some(ErrorKind::ArgumentConflict)
            );
        }
        let logger =
            new().try_get_matches_from(["immortal", "--config", "run.yml", "--logger", "/bin/cat"]);
        assert_eq!(
            logger.err().map(|error| error.kind()),
            Some(ErrorKind::ArgumentConflict)
        );
    }

    #[test]
    fn child_options_after_command_are_preserved() {
        let result = new().try_get_matches_from([
            "immortal",
            "--name",
            "child-options",
            "sh",
            "-c",
            "echo ready",
            "--check-config",
            "-cc",
            "-ctl",
            "-v",
            "-e",
            "/tmp/env",
            "--logger",
            "child-argument",
        ]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };
        let command = matches
            .get_many::<String>("command")
            .map(|values| values.map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();

        assert_eq!(
            command,
            [
                "sh",
                "-c",
                "echo ready",
                "--check-config",
                "-cc",
                "-ctl",
                "-v",
                "-e",
                "/tmp/env",
                "--logger",
                "child-argument",
            ]
        );
    }

    #[test]
    fn retries_default_matches_go_version() {
        let result = new().try_get_matches_from(["immortal", "--name", "true-test", "true"]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };

        assert_eq!(matches.get_one::<i32>("retries"), Some(&-1));
    }

    #[test]
    fn direct_command_requires_name_or_control_directory() {
        let missing = new().try_get_matches_from(["immortal", "sleep", "30"]);
        assert_eq!(
            missing.err().map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument)
        );

        assert!(
            new()
                .try_get_matches_from(["immortal", "--name", "sleep", "sleep", "30"])
                .is_ok()
        );
        assert!(
            new()
                .try_get_matches_from([
                    "immortal",
                    "--control-dir",
                    "/run/immortal/sleep",
                    "sleep",
                    "30",
                ])
                .is_ok()
        );
    }

    #[test]
    fn name_conflicts_with_config_and_exact_control_directory() {
        for result in [
            new().try_get_matches_from(["immortal", "--name", "sleep", "--config", "sleep.yml"]),
            new().try_get_matches_from([
                "immortal",
                "--name",
                "sleep",
                "--control-dir",
                "/run/immortal/sleep",
                "sleep",
                "30",
            ]),
        ] {
            assert_eq!(
                result.err().map(|error| error.kind()),
                Some(ErrorKind::ArgumentConflict)
            );
        }
    }
}
