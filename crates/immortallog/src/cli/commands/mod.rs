//! Clap syntax for streaming file logging and archive inspection.

use std::path::PathBuf;

use clap::{Arg, ArgAction, ColorChoice, Command, ValueHint};

/// Build the command-line interface.
#[must_use]
pub fn new() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(immortal_core::build_info::long_version())
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .color(ColorChoice::Auto)
        .args_conflicts_with_subcommands(true)
        .disable_help_subcommand(true)
        .subcommand_negates_reqs(true)
        .subcommand(command_archives())
        .arg(
            Arg::new("file")
                .value_name("FILE")
                .value_hint(ValueHint::FilePath)
                .value_parser(clap::value_parser!(PathBuf))
                .required(true)
                .help("Append stdin to FILE"),
        )
        .arg(limit("max-bytes", "Rotate before a write exceeds BYTES"))
        .arg(limit("max-age", "Rotate a nonempty file after SECONDS"))
        .arg(
            Arg::new("keep")
                .long("keep")
                .value_name("COUNT")
                .value_parser(clap::value_parser!(u32).range(1..))
                .help("Retain at most COUNT Immortal archives"),
        )
        .arg(limit(
            "max-total-bytes",
            "Retain at most BYTES across Immortal archives",
        ))
        .arg(
            Arg::new("timestamp")
                .long("timestamp")
                .action(ArgAction::SetTrue)
                .help("Prefix each logical file record with Unix time"),
        )
        .arg(
            Arg::new("passthrough")
                .long("passthrough")
                .action(ArgAction::SetTrue)
                .help("Copy original bytes to stdout for a following logger stage"),
        )
}

fn command_archives() -> Command {
    Command::new("archives")
        .about("List archives owned by a live-file namespace")
        .arg(
            Arg::new("output")
                .short('o')
                .long("output")
                .value_name("FORMAT")
                .value_parser(["table", "json"])
                .default_value("table")
                .help("Select table or JSON output"),
        )
        .arg(
            Arg::new("file")
                .value_name("FILE")
                .value_hint(ValueHint::FilePath)
                .value_parser(clap::value_parser!(PathBuf))
                .required(true)
                .help("Live file whose archives should be listed"),
        )
}

fn limit(name: &'static str, help: &'static str) -> Arg {
    Arg::new(name)
        .long(name)
        .value_name(if name == "max-age" {
            "SECONDS"
        } else {
            "BYTES"
        })
        .value_parser(clap::value_parser!(u64).range(1..))
        .help(help)
}

#[cfg(test)]
mod tests {
    use clap::error::ErrorKind;

    use super::new;

    #[test]
    fn definition_is_valid() {
        new().debug_assert();
    }

    #[test]
    fn version_includes_the_source_revision() {
        let command = new();
        assert_eq!(
            command.get_long_version(),
            Some(immortal_core::build_info::long_version())
        );

        let short = new()
            .try_get_matches_from(["immortallog", "-V"])
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

        let long = command
            .try_get_matches_from(["immortallog", "--version"])
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
    }

    #[test]
    fn limits_are_positive_and_typed() {
        let valid = new().try_get_matches_from([
            "immortallog",
            "--max-bytes",
            "1024",
            "--keep",
            "7",
            "--passthrough",
            "/tmp/api.log",
        ]);
        assert!(valid.is_ok());
        let invalid =
            new().try_get_matches_from(["immortallog", "--max-bytes", "0", "/tmp/api.log"]);
        assert_eq!(
            invalid.err().map(|error| error.kind()),
            Some(ErrorKind::ValueValidation)
        );
    }

    #[test]
    fn archives_accepts_typed_output_and_requires_a_file() {
        let valid = new().try_get_matches_from([
            "immortallog",
            "archives",
            "--output",
            "json",
            "/tmp/api.log",
        ]);
        assert!(valid.is_ok());

        let invalid_output = new().try_get_matches_from([
            "immortallog",
            "archives",
            "--output",
            "yaml",
            "/tmp/api.log",
        ]);
        assert_eq!(
            invalid_output.err().map(|error| error.kind()),
            Some(ErrorKind::InvalidValue)
        );

        let missing_file = new().try_get_matches_from(["immortallog", "archives"]);
        assert_eq!(
            missing_file.err().map(|error| error.kind()),
            Some(ErrorKind::MissingRequiredArgument)
        );
    }

    #[test]
    fn explicit_relative_path_disambiguates_a_file_named_archives() {
        let matches = new().try_get_matches_from(["immortallog", "./archives"]);
        assert!(matches.is_ok_and(|matches| matches.subcommand_name().is_none()));
    }

    #[test]
    fn archive_mode_rejects_writer_options() {
        let matches = new().try_get_matches_from([
            "immortallog",
            "--max-bytes",
            "1024",
            "archives",
            "/tmp/api.log",
        ]);
        assert_eq!(
            matches.err().map(|error| error.kind()),
            Some(ErrorKind::ArgumentConflict)
        );
    }

    #[test]
    fn bare_help_remains_a_writer_destination() {
        let matches = new().try_get_matches_from(["immortallog", "help"]);
        assert!(matches.is_ok_and(|matches| matches.subcommand_name().is_none()));
    }
}
