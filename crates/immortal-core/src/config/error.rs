//! Configuration failure type shared by parsing, environment loading, and validation.
//!
//! [`ConfigError`] is the single error surface returned by every public
//! configuration entry point. Its variants distinguish size, I/O,
//! environment-input, parse, version, and validation failures so callers can
//! react to each precisely instead of downcasting a generic error.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use super::MAX_CONFIG_BYTES;

/// Configuration parsing or validation failure.
#[derive(Debug)]
pub enum ConfigError {
    /// Input exceeded [`MAX_CONFIG_BYTES`].
    TooLarge { actual: u64 },
    /// The file could not be read.
    Io(io::Error),
    /// A direct-command environment directory or one of its entries was invalid.
    EnvironmentInput {
        /// Path which could not be inspected or decoded.
        path: PathBuf,
        /// Operating-system or bounded-input failure.
        source: io::Error,
    },
    /// YAML was malformed or did not match the selected schema.
    Parse(String),
    /// The required configuration version marker was absent.
    MissingVersion,
    /// A version marker was present but unsupported.
    UnsupportedVersion(u64),
    /// The document parsed but violated runtime invariants.
    Validation(Vec<String>),
}

impl Display for ConfigError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { actual } => write!(
                formatter,
                "configuration is {actual} bytes; limit is {MAX_CONFIG_BYTES} bytes"
            ),
            Self::Io(error) => write!(formatter, "unable to read configuration: {error}"),
            Self::EnvironmentInput { path, source } => {
                write!(
                    formatter,
                    "unable to load environment input `{}`: {source}",
                    path.display()
                )
            }
            Self::Parse(error) => write!(formatter, "invalid YAML configuration: {error}"),
            Self::MissingVersion => formatter.write_str(
                "configuration must declare `version: 2`; unversioned Go configuration is intentionally unsupported; see INSTALL.md#definition-migration",
            ),
            Self::UnsupportedVersion(version) => {
                write!(
                    formatter,
                    "unsupported configuration version {version}; only version 2 is accepted"
                )
            }
            Self::Validation(errors) => {
                write!(
                    formatter,
                    "invalid service configuration: {}",
                    errors.join("; ")
                )
            }
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::EnvironmentInput { source, .. } => Some(source),
            Self::TooLarge { .. }
            | Self::Parse(_)
            | Self::MissingVersion
            | Self::UnsupportedVersion(_)
            | Self::Validation(_) => None,
        }
    }
}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
