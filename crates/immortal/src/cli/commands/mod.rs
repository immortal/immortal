//! Clap command and option definitions for `immortal`.

use std::ffi::{OsStr, OsString};

use clap::{
    Arg, ArgAction, ArgGroup, ArgMatches, ColorChoice, Command, Error, ValueHint,
    builder::styling::{AnsiColor, Effects, Styles},
};

const CONFIG_CONFLICTS: [&str; 11] = [
    "retries",
    "child-pid",
    "env-dir",
    "follow-pid",
    "log-file",
    "logger",
    "name",
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
             backed by process contracts. Options whose lifecycle is not implemented fail \
             explicitly.",
        )
        .override_usage("immortal [OPTIONS] <COMMAND> [ARGUMENTS]...")
        .after_help(
            "Examples:
  immortal --foreground --retries 0 /bin/true
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
        .arg(arg_retries())
        .arg(arg_check_config())
        .arg(arg_child_pid())
        .arg(arg_config())
        .arg(arg_control_dir())
        .arg(arg_env_dir())
        .arg(arg_follow_pid())
        .arg(arg_log_file())
        .arg(arg_logger())
        .arg(arg_name())
        .arg(arg_supervisor_pid())
        .arg(arg_user())
        .arg(arg_working_dir())
        .arg(arg_wait())
        .arg(arg_command())
}

/// Parse arguments after translating released multi-character single-dash flags.
///
/// Translation stops at the child command so option-looking child arguments
/// remain byte-for-byte unchanged.
///
/// # Errors
///
/// Returns Clap's structured syntax or validation error.
pub fn try_get_matches_from<I, T>(arguments: I) -> Result<ArgMatches, Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    let mut normalized = Vec::with_capacity(arguments.len());
    let mut expects_value = false;
    let mut command_started = false;
    for (position, argument) in arguments.into_iter().enumerate() {
        if position == 0 || command_started {
            normalized.push(argument);
            continue;
        }
        if expects_value {
            expects_value = false;
            normalized.push(argument);
            continue;
        }
        if takes_value(&argument) {
            expects_value = true;
        } else if !argument.as_encoded_bytes().starts_with(b"-") {
            command_started = true;
        }
        normalized.push(normalize_legacy_option(argument));
    }
    new().try_get_matches_from(normalized)
}

fn takes_value(argument: &OsStr) -> bool {
    matches!(
        argument.to_str(),
        Some(
            "-r" | "--retries"
                | "-p"
                | "--child-pid"
                | "-c"
                | "--config"
                | "-ctl"
                | "--control-dir"
                | "-e"
                | "--env-dir"
                | "-f"
                | "--follow-pid"
                | "-l"
                | "--log-file"
                | "-logger"
                | "--logger"
                | "-name"
                | "--name"
                | "-P"
                | "--supervisor-pid"
                | "-u"
                | "--user"
                | "-d"
                | "--working-dir"
                | "-w"
                | "--wait"
        )
    )
}

fn normalize_legacy_option(argument: OsString) -> OsString {
    match argument.to_str() {
        Some("-cc") => OsString::from("--check-config"),
        Some("-ctl") => OsString::from("--control-dir"),
        Some("-logger") => OsString::from("--logger"),
        Some("-name") => OsString::from("--name"),
        Some("-v") => OsString::from("--version"),
        Some(_) | None => argument,
    }
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
        .short('n')
        .long("foreground")
        .help("Stay in the foreground instead of daemonizing")
        .action(ArgAction::SetTrue)
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
        .conflicts_with("name")
}

fn arg_env_dir() -> Arg {
    Arg::new("env-dir")
        .short('e')
        .long("env-dir")
        .value_name("DIR")
        .value_hint(ValueHint::DirPath)
        .help("Set environment variables from files in DIR")
        .conflicts_with("config")
}

fn arg_follow_pid() -> Arg {
    Arg::new("follow-pid")
        .short('f')
        .long("follow-pid")
        .value_name("PIDFILE")
        .value_hint(ValueHint::FilePath)
        .help("Follow the PID written by a daemonizing child to PIDFILE")
        .conflicts_with("config")
}

fn arg_log_file() -> Arg {
    Arg::new("log-file")
        .short('l')
        .long("log-file")
        .value_name("FILE")
        .value_hint(ValueHint::FilePath)
        .help("Write combined stdout and stderr to FILE")
        .conflicts_with("config")
}

fn arg_logger() -> Arg {
    Arg::new("logger")
        .long("logger")
        .value_name("COMMAND")
        .value_hint(ValueHint::CommandString)
        .help("Pipe combined stdout and stderr to COMMAND")
        .conflicts_with("config")
}

fn arg_name() -> Arg {
    Arg::new("name")
        .long("name")
        .value_name("SERVICE")
        .help("Use SERVICE instead of the supervisor PID under $HOME/.immortal")
        .conflicts_with_all(["config", "control-dir"])
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
        .value_parser(clap::value_parser!(u64))
        .conflicts_with("config")
}

fn arg_command() -> Arg {
    Arg::new("command")
        .value_name("COMMAND")
        .value_hint(ValueHint::CommandWithArguments)
        .help("Command and arguments to supervise")
        .num_args(1..)
        .allow_hyphen_values(true)
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
    fn help_is_available() {
        let result = new().try_get_matches_from(["immortal", "--help"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayHelp)
        );
    }

    #[test]
    fn version_is_available() {
        let short = try_get_matches_from(["immortal", "-V"])
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

        let long = try_get_matches_from(["immortal", "--version"])
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
        for option in ["-v", "-V", "--version"] {
            let result = try_get_matches_from(["immortal", option]);
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
            "-n",
            "-r",
            "2",
            "-p",
            "/tmp/child.pid",
            "-e",
            "/tmp/env",
            "-f",
            "/tmp/follow.pid",
            "-l",
            "/tmp/service.log",
            "-P",
            "/tmp/supervisor.pid",
            "-u",
            "www",
            "-d",
            "/tmp",
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
            "--logger",
            "logger -t service",
            "--name",
            "service",
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
            matches.get_one::<String>("name").map(String::as_str),
            Some("service")
        );
    }

    #[test]
    fn config_check_requires_config() {
        let valid =
            new().try_get_matches_from(["immortal", "--config", "run.yml", "--check-config"]);
        assert!(valid.is_ok());

        let invalid = new().try_get_matches_from(["immortal", "--check-config"]);
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument)
        );
    }

    #[test]
    fn released_multi_character_flags_are_accepted() {
        let check = try_get_matches_from(["immortal", "-c", "run.yml", "-cc"]);
        assert!(check.is_ok());
        let control = try_get_matches_from([
            "immortal",
            "-ctl",
            "/tmp/control",
            "-logger",
            "logger -t api",
            "/bin/true",
        ]);
        assert!(control.is_ok());
        let name = try_get_matches_from(["immortal", "-name", "api", "/bin/true"]);
        assert!(name.is_ok());
    }

    #[test]
    fn legacy_options_after_child_command_are_not_rewritten() {
        let matches = try_get_matches_from(["immortal", "/bin/echo", "-logger", "child"]);
        assert!(matches.is_ok());
        let Some(matches) = matches.ok() else {
            return;
        };
        let command = matches
            .get_many::<String>("command")
            .map(|values| values.map(String::as_str).collect::<Vec<_>>())
            .unwrap_or_default();
        assert_eq!(command, ["/bin/echo", "-logger", "child"]);
    }

    #[test]
    fn config_rejects_ignored_command_options() {
        let result =
            new().try_get_matches_from(["immortal", "--config", "run.yml", "--retries", "2"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::ArgumentConflict)
        );
    }

    #[test]
    fn child_options_after_command_are_preserved() {
        let result = new().try_get_matches_from([
            "immortal",
            "sh",
            "-c",
            "echo ready",
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
            ["sh", "-c", "echo ready", "--logger", "child-argument"]
        );
    }

    #[test]
    fn retries_default_matches_go_version() {
        let result = new().try_get_matches_from(["immortal", "true"]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };

        assert_eq!(matches.get_one::<i32>("retries"), Some(&-1));
    }
}
