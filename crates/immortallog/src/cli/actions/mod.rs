//! Typed logging actions and focused operation handlers.
//!
//! Dispatch selects either streaming or archive inspection. The named binary
//! routes that variant to the corresponding module, while rotation and archive
//! ownership remain implemented by `immortal-core`.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    path::PathBuf,
};

use immortal_core::{exit::ExitClass, logging::RotationPolicy};

pub mod archives;
pub mod write;

/// Stable archive inspection output representation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputFormat {
    /// Human-readable space-aligned table.
    Table,
    /// Machine-readable JSON array.
    Json,
}

/// Fully typed file adapter action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteAction {
    /// Destination file.
    pub file: PathBuf,
    /// Rotation and retention limits.
    pub rotation: RotationPolicy,
    /// Prefix logical file records with a timestamp.
    pub timestamp: bool,
    /// Copy original bytes to stdout after durable file writes.
    pub passthrough: bool,
}

/// Fully typed archive inspection action.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArchivesAction {
    /// Live file whose sibling archives should be listed.
    pub file: PathBuf,
    /// Output representation.
    pub output: OutputFormat,
}

/// One mutually exclusive `immortallog` operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Action {
    /// Stream stdin into one rotating file.
    Write(WriteAction),
    /// Inspect archives for one live-file namespace.
    Archives(ArchivesAction),
}

/// File adapter or archive inspection failure.
#[derive(Debug)]
pub enum ActionError {
    /// Streaming or rotation failed.
    Adapter(io::Error),
    /// Archive discovery or output failed.
    Archives(io::Error),
    /// Archive timestamp exceeds the UTC formatter's supported range.
    TimestampRange(u128),
    /// Archive timestamp could not be converted to UTC.
    Timestamp {
        /// Raw timestamp from the archive name.
        unix_nanoseconds: u128,
        /// Conversion failure.
        source: jiff::Error,
    },
    /// Archive path cannot be represented by the UTF-8 output contracts.
    NonUtf8Path,
    /// JSON serialization failed.
    Json(serde_json::Error),
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Adapter(error) => write!(formatter, "logging adapter failed: {error}"),
            Self::Archives(error) => write!(formatter, "archive inspection failed: {error}"),
            Self::TimestampRange(unix_nanoseconds) => write!(
                formatter,
                "archive timestamp {unix_nanoseconds} is outside the supported UTC range"
            ),
            Self::Timestamp {
                unix_nanoseconds,
                source,
            } => write!(
                formatter,
                "archive timestamp {unix_nanoseconds} cannot be converted to UTC: {source}"
            ),
            Self::NonUtf8Path => formatter.write_str("archive path is not valid UTF-8"),
            Self::Json(error) => write!(formatter, "archive JSON output failed: {error}"),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Adapter(error) | Self::Archives(error) => Some(error),
            Self::Timestamp { source, .. } => Some(source),
            Self::Json(error) => Some(error),
            Self::TimestampRange(_) | Self::NonUtf8Path => None,
        }
    }
}

impl ActionError {
    /// Stable process exit classification.
    #[must_use]
    pub fn exit_class(&self) -> ExitClass {
        match self {
            Self::Adapter(_) => ExitClass::IoError,
            Self::Archives(error) => match error.kind() {
                io::ErrorKind::NotFound => ExitClass::NotFound,
                io::ErrorKind::PermissionDenied => ExitClass::Permission,
                _ => ExitClass::IoError,
            },
            Self::TimestampRange(_) | Self::Timestamp { .. } | Self::NonUtf8Path => ExitClass::Data,
            Self::Json(error) => {
                if error.is_io() {
                    ExitClass::IoError
                } else {
                    ExitClass::Software
                }
            }
        }
    }
}
