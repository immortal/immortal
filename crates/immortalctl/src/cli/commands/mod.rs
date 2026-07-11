//! Clap command and option definitions for `immortalctl`.

use std::ffi::{OsStr, OsString};

use clap::{
    Arg, ArgAction, ArgGroup, ArgMatches, ColorChoice, Command, Error, ValueHint,
    builder::{
        PossibleValuesParser,
        styling::{AnsiColor, Effects, Styles},
    },
};

const SIGNALS: [&str; 13] = [
    "alrm", "cont", "hup", "int", "kill", "quit", "stop", "term", "ttin", "ttou", "usr1", "usr2",
    "winch",
];

/// Build the command-line interface.
#[must_use]
pub fn new() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .long_about(
            "Inspect immortal supervisors and request lifecycle or signal operations. With no \
             subcommand, status for all safely discoverable services is selected. Lifecycle \
             commands wait for typed completion by default; --no-wait returns after acceptance.",
        )
        .after_help(
            "Examples:
  immortalctl
  immortalctl status api
  immortalctl restart api
  immortalctl stop --all
  immortalctl signal usr2 worker",
        )
        .color(ColorChoice::Auto)
        .styles(styles())
        .disable_help_flag(true)
        .disable_help_subcommand(true)
        .arg(arg_help())
        .arg(arg_runtime_dir())
        .arg(arg_output())
        .arg(arg_color())
        .arg(arg_no_header())
        .arg(arg_timeout())
        .arg(arg_no_wait())
        .arg(arg_legacy_signal())
        .arg(arg_legacy_target())
        .subcommand(command_status())
        .subcommand(service_command("start", "Start a stopped service"))
        .subcommand(service_command(
            "stop",
            "Stop a service without exiting its supervisor",
        ))
        .subcommand(service_command(
            "restart",
            "Restart a service without replacing its supervisor",
        ))
        .subcommand(service_command(
            "once",
            "Start a service without restarting it after its next exit",
        ))
        .subcommand(service_command(
            "exit",
            "Exit a supervisor without stopping a followed process",
        ))
        .subcommand(service_command(
            "halt",
            "Stop a service and exit its supervisor",
        ))
        .subcommand(command_signal())
}

/// Parse arguments after translating released multi-character short flags.
///
/// # Errors
///
/// Returns Clap's structured error for invalid syntax or conflicting legacy
/// signal flags.
pub fn try_get_matches_from<I, T>(arguments: I) -> Result<ArgMatches, Error>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString> + Clone,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    let has_subcommand = arguments.iter().skip(1).any(|argument| {
        matches!(
            argument.to_str(),
            Some("status" | "start" | "stop" | "restart" | "once" | "exit" | "halt" | "signal")
        )
    });
    let mut normalized = Vec::with_capacity(arguments.len());
    for (position, argument) in arguments.into_iter().enumerate() {
        if position == 0 || has_subcommand {
            normalized.push(argument);
            continue;
        }
        if let Some(signal) = legacy_signal_name(&argument) {
            normalized.push(OsString::from("--legacy-signal"));
            normalized.push(OsString::from(signal));
        } else if argument == OsStr::new("-A") {
            normalized.push(OsString::from("--color=never"));
        } else if argument == OsStr::new("-v") {
            normalized.push(OsString::from("--version"));
        } else {
            normalized.push(argument);
        }
    }
    new().try_get_matches_from(normalized)
}

fn legacy_signal_name(argument: &OsStr) -> Option<&'static str> {
    match argument.to_str() {
        Some("-1") => Some("usr1"),
        Some("-2") => Some("usr2"),
        Some("-a") => Some("alrm"),
        Some("-c") => Some("cont"),
        Some("-h") => Some("hup"),
        Some("-i") => Some("int"),
        Some("-k") => Some("kill"),
        Some("-in") => Some("ttin"),
        Some("-ou") => Some("ttou"),
        Some("-q") => Some("quit"),
        Some("-s") => Some("stop"),
        Some("-t") => Some("term"),
        Some("-w") => Some("winch"),
        Some(_) | None => None,
    }
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}

fn arg_help() -> Arg {
    Arg::new("help")
        .long("help")
        .help("Print help")
        .action(ArgAction::Help)
}

fn arg_legacy_signal() -> Arg {
    Arg::new("legacy-signal")
        .long("legacy-signal")
        .hide(true)
        .value_parser(PossibleValuesParser::new(SIGNALS))
        .action(ArgAction::Append)
        .requires("legacy-target")
}

fn arg_legacy_target() -> Arg {
    Arg::new("legacy-target")
        .value_name("SERVICE")
        .hide(true)
        .num_args(0..=1)
        .requires("legacy-signal")
}

fn arg_runtime_dir() -> Arg {
    Arg::new("runtime-dir")
        .long("runtime-dir")
        .value_name("DIR")
        .value_hint(ValueHint::DirPath)
        .env("IMMORTAL_SDIR")
        .default_value("/var/run/immortal")
        .help("Discover system supervisors under DIR")
        .global(true)
}

fn arg_output() -> Arg {
    Arg::new("output")
        .short('o')
        .long("output")
        .value_name("FORMAT")
        .value_parser(["table", "json"])
        .default_value("table")
        .help("Select the output format")
        .global(true)
}

fn arg_color() -> Arg {
    Arg::new("color")
        .long("color")
        .value_name("WHEN")
        .value_parser(["auto", "always", "never"])
        .default_value("auto")
        .help("Control colors in command output")
        .global(true)
}

fn arg_no_header() -> Arg {
    Arg::new("no-header")
        .long("no-header")
        .help("Omit the table header")
        .action(ArgAction::SetTrue)
        .global(true)
        .conflicts_with("output")
}

fn arg_timeout() -> Arg {
    Arg::new("timeout")
        .long("timeout")
        .value_name("SECONDS")
        .value_parser(clap::value_parser!(u64).range(1..))
        .default_value("30")
        .help("Maximum time to wait for lifecycle completion")
        .global(true)
}

fn arg_no_wait() -> Arg {
    Arg::new("no-wait")
        .long("no-wait")
        .help("Return after a lifecycle request is accepted")
        .action(ArgAction::SetTrue)
        .global(true)
}

fn command_status() -> Command {
    Command::new("status")
        .about("Show service status")
        .arg(arg_service(false))
        .arg(arg_all())
}

fn service_command(name: &'static str, about: &'static str) -> Command {
    Command::new(name)
        .about(about)
        .arg(arg_service(false))
        .arg(arg_all())
        .group(
            ArgGroup::new("target")
                .args(["service", "all"])
                .required(true),
        )
}

fn command_signal() -> Command {
    Command::new("signal")
        .about("Send a Unix signal to a supervised service")
        .arg(
            Arg::new("signal")
                .value_name("SIGNAL")
                .help("Signal name")
                .required(true)
                .ignore_case(true)
                .value_parser(PossibleValuesParser::new(SIGNALS)),
        )
        .arg(
            Arg::new("scope")
                .long("scope")
                .value_name("SCOPE")
                .value_parser(["main", "group"])
                .default_value("main")
                .help("Target the main process or its owned process group"),
        )
        .arg(arg_service(false))
        .arg(arg_all())
        .group(
            ArgGroup::new("target")
                .args(["service", "all"])
                .required(true),
        )
}

fn arg_service(required: bool) -> Arg {
    Arg::new("service")
        .value_name("SERVICE")
        .help("Service name")
        .required(required)
        .conflicts_with("all")
}

fn arg_all() -> Arg {
    Arg::new("all")
        .short('a')
        .long("all")
        .help("Target all discovered services")
        .action(ArgAction::SetTrue)
        .conflicts_with("service")
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
        let result = try_get_matches_from(["immortalctl", "--help"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayHelp)
        );
    }

    #[test]
    fn version_is_available() {
        let result = new().try_get_matches_from(["immortalctl", "--version"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayVersion)
        );
    }

    #[test]
    fn no_subcommand_selects_default_status_path() {
        let result = new().try_get_matches_from(["immortalctl"]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };

        assert!(matches.subcommand().is_none());
    }

    #[test]
    fn lifecycle_commands_require_a_target() {
        for action in ["start", "stop", "restart", "once", "exit", "halt"] {
            let missing = new().try_get_matches_from(["immortalctl", action]);
            assert_eq!(
                missing.err().map(|error| error.kind()),
                Some(ErrorKind::MissingRequiredArgument)
            );

            let service = new().try_get_matches_from(["immortalctl", action, "api"]);
            assert!(service.is_ok());

            let all = new().try_get_matches_from(["immortalctl", action, "--all"]);
            assert!(all.is_ok());
        }
    }

    #[test]
    fn signal_names_are_typed() {
        let valid = new().try_get_matches_from(["immortalctl", "signal", "USR2", "worker"]);
        assert!(valid.is_ok());

        let invalid = new().try_get_matches_from(["immortalctl", "signal", "invalid", "worker"]);
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ErrorKind::InvalidValue)
        );
    }

    #[test]
    fn output_options_are_typed() {
        let result = new().try_get_matches_from([
            "immortalctl",
            "--output",
            "json",
            "--color",
            "never",
            "status",
            "api",
        ]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };

        assert_eq!(
            matches.get_one::<String>("output").map(String::as_str),
            Some("json")
        );
        assert_eq!(
            matches.get_one::<String>("color").map(String::as_str),
            Some("never")
        );
    }

    #[test]
    fn lifecycle_wait_options_are_typed_and_positive() {
        let matches = new()
            .try_get_matches_from([
                "immortalctl",
                "restart",
                "api",
                "--timeout",
                "12",
                "--no-wait",
            ])
            .ok();
        assert_eq!(
            matches
                .as_ref()
                .and_then(|matches| matches.get_one::<u64>("timeout")),
            Some(&12)
        );
        assert!(matches.is_some_and(|matches| matches.get_flag("no-wait")));

        let invalid =
            new().try_get_matches_from(["immortalctl", "restart", "api", "--timeout", "0"]);
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ErrorKind::ValueValidation)
        );
    }

    #[test]
    fn all_released_signal_flags_are_normalized() {
        for (option, expected) in [
            ("-1", "usr1"),
            ("-2", "usr2"),
            ("-a", "alrm"),
            ("-c", "cont"),
            ("-h", "hup"),
            ("-i", "int"),
            ("-k", "kill"),
            ("-in", "ttin"),
            ("-ou", "ttou"),
            ("-q", "quit"),
            ("-s", "stop"),
            ("-t", "term"),
            ("-w", "winch"),
        ] {
            let result = try_get_matches_from(["immortalctl", option, "api"]);
            assert!(result.is_ok());
            let Some(matches) = result.ok() else {
                return;
            };
            assert_eq!(
                matches
                    .get_many::<String>("legacy-signal")
                    .and_then(|mut values| values.next())
                    .map(String::as_str),
                Some(expected)
            );
            assert_eq!(
                matches
                    .get_one::<String>("legacy-target")
                    .map(String::as_str),
                Some("api")
            );
        }
    }

    #[test]
    fn modern_all_flag_is_not_rewritten_as_alarm() {
        let result = try_get_matches_from(["immortalctl", "stop", "--all"]);
        assert!(result.is_ok());
    }

    #[test]
    fn hup_does_not_replace_long_help() {
        let hup = try_get_matches_from(["immortalctl", "-h", "api"]);
        assert!(hup.is_ok());
        let help = try_get_matches_from(["immortalctl", "--help"]);
        assert_eq!(
            help.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayHelp)
        );
    }
}
