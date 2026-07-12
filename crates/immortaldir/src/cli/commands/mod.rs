//! Clap command and option definitions for `immortaldir`.

use clap::{
    Arg, ArgAction, ColorChoice, Command, ValueHint,
    builder::styling::{AnsiColor, Effects, Styles},
};

/// Build the command-line interface.
#[must_use]
pub fn new() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(immortal_core::build_info::long_version())
        .author(env!("CARGO_PKG_AUTHORS"))
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .long_about(
            "Reconcile immortal service definitions from a directory. Each authoritative scan \
             can start missing supervisors, preserve unchanged or operator-stopped services, \
             apply valid changes, stop disabled services, and halt confirmed deletions. Dry-run \
             mode reports the same desired-state plan without changing supervisors.",
        )
        .after_help(
            "Examples:
  immortaldir /etc/immortal
  immortaldir /srv/immortal
  immortaldir --once --dry-run ./services",
        )
        .color(ColorChoice::Auto)
        .styles(styles())
        .arg_required_else_help(true)
        .disable_help_subcommand(true)
        .arg(arg_directory())
        .arg(arg_runtime_dir())
        .arg(arg_scan_interval())
        .arg(arg_max_concurrent_starts())
        .arg(arg_supervisor_binary())
        .arg(arg_once())
        .arg(arg_dry_run())
}

fn styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::Yellow.on_default() | Effects::BOLD)
        .usage(AnsiColor::Green.on_default() | Effects::BOLD)
        .literal(AnsiColor::Blue.on_default() | Effects::BOLD)
        .placeholder(AnsiColor::Green.on_default())
}

fn arg_directory() -> Arg {
    Arg::new("directory")
        .value_name("DIRECTORY")
        .value_hint(ValueHint::DirPath)
        .help("Directory containing immortal service definitions")
        .required(true)
}

fn arg_runtime_dir() -> Arg {
    Arg::new("runtime-dir")
        .long("runtime-dir")
        .value_name("DIR")
        .value_hint(ValueHint::DirPath)
        .env("IMMORTAL_SDIR")
        .default_value(immortal_core::runtime::system_runtime_root().as_os_str())
        .help("Store and discover supervisor state under DIR")
}

fn arg_scan_interval() -> Arg {
    Arg::new("scan-interval")
        .long("scan-interval")
        .value_name("SECONDS")
        .default_value("30")
        .help("Maximum delay between reconciliation scans")
        .value_parser(clap::value_parser!(u64).range(1..))
}

fn arg_max_concurrent_starts() -> Arg {
    Arg::new("max-concurrent-starts")
        .long("max-concurrent-starts")
        .value_name("COUNT")
        .env("IMMORTAL_MAX_CONCURRENT_STARTS")
        .help(format!(
            "Maximum supervisors launched concurrently within one dependency wave (default: {})",
            immortal_core::reconcile::DEFAULT_MAX_CONCURRENT_LAUNCHES
        ))
        .value_parser(
            clap::value_parser!(u64)
                .range(1..=immortal_core::reconcile::MAX_CONCURRENT_LAUNCHES as u64),
        )
}

fn arg_supervisor_binary() -> Arg {
    Arg::new("supervisor-binary")
        .long("supervisor-binary")
        .value_name("PATH")
        .value_hint(ValueHint::ExecutablePath)
        .env("IMMORTAL_BIN")
        .default_value("immortal")
        .help("Executable used to launch new immortal supervisors")
}

fn arg_once() -> Arg {
    Arg::new("once")
        .long("once")
        .help("Reconcile once and exit instead of watching for changes")
        .action(ArgAction::SetTrue)
}

fn arg_dry_run() -> Arg {
    Arg::new("dry-run")
        .long("dry-run")
        .help("Print the reconciliation plan without changing supervisors")
        .action(ArgAction::SetTrue)
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
        let result = new().try_get_matches_from(["immortaldir", "--help"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayHelp)
        );
    }

    #[test]
    fn version_is_available() {
        let short = new()
            .try_get_matches_from(["immortaldir", "-V"])
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
            .try_get_matches_from(["immortaldir", "--version"])
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
        let result = new().try_get_matches_from(["immortaldir", "--version"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::DisplayVersion)
        );
    }

    #[test]
    fn directory_is_required() {
        let result = new().try_get_matches_from(["immortaldir", "--once"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument)
        );
    }

    #[test]
    fn reconciliation_options_are_typed() {
        let result = new().try_get_matches_from([
            "immortaldir",
            "--runtime-dir",
            "/tmp/run",
            "--scan-interval",
            "10",
            "--max-concurrent-starts",
            "4",
            "--once",
            "--dry-run",
            "/tmp/services",
        ]);
        assert!(result.is_ok());
        let Some(matches) = result.ok() else {
            return;
        };

        assert_eq!(matches.get_one::<u64>("scan-interval"), Some(&10));
        assert_eq!(matches.get_one::<u64>("max-concurrent-starts"), Some(&4));
        assert!(matches.get_flag("once"));
        assert!(matches.get_flag("dry-run"));
    }

    #[test]
    fn scan_interval_must_be_positive() {
        let result =
            new().try_get_matches_from(["immortaldir", "--scan-interval", "0", "/tmp/services"]);
        assert_eq!(
            result.err().map(|error| error.kind()),
            Some(ErrorKind::ValueValidation)
        );
    }

    #[test]
    fn concurrent_start_limit_is_bounded() {
        for value in ["0", "65"] {
            let result = new().try_get_matches_from([
                "immortaldir",
                "--max-concurrent-starts",
                value,
                "/tmp/services",
            ]);
            assert_eq!(
                result.err().map(|error| error.kind()),
                Some(ErrorKind::ValueValidation)
            );
        }
    }

    #[test]
    fn dry_run_can_watch_without_mutation() {
        let result = new().try_get_matches_from(["immortaldir", "--dry-run", "/tmp/services"]);
        assert!(result.is_ok());
    }
}
