//! Clap syntax for the file logger compatibility adapter.

use clap::{Arg, ArgAction, ColorChoice, Command, ValueHint};

/// Build the command-line interface.
#[must_use]
pub fn new() -> Command {
    Command::new(env!("CARGO_PKG_NAME"))
        .version(env!("CARGO_PKG_VERSION"))
        .long_version(immortal_core::build_info::long_version())
        .about(env!("CARGO_PKG_DESCRIPTION"))
        .color(ColorChoice::Auto)
        .arg(
            Arg::new("file")
                .value_name("FILE")
                .value_hint(ValueHint::FilePath)
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
}
