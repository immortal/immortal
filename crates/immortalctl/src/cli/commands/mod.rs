//! Clap command and option definitions for `immortalctl`.

use clap::{
    Arg, ArgAction, ArgGroup, ColorChoice, Command, ValueHint,
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
             subcommand, status for all discoverable services is selected. The Rust rewrite \
             currently defines this interface but does not yet contact supervisors.",
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
        .disable_help_subcommand(true)
        .arg(arg_runtime_dir())
        .arg(arg_output())
        .arg(arg_color())
        .arg(arg_no_header())
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

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
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

    use super::new;

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
        let result = new().try_get_matches_from(["immortalctl", "--help"]);
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
}
