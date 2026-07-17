//! Typed failures for logging planning, runtime lookup, shutdown, and retention.
//!
//! The error type is shared by the private logging children but re-exported by
//! the facade so callers continue to handle all logging failures through the
//! canonical `immortal_core::logging::LoggingError` path.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
};

/// Invalid logger plan, runtime lookup, or shutdown transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggingError {
    /// External logger argv has no executable.
    EmptyExternalCommand,
    /// Selected local-file routing has no stream.
    EmptyFileSelection,
    /// Runtime stream does not have a pipeline.
    UnknownPipeline,
    /// Pipeline stage index does not exist.
    UnknownStage,
    /// Runtime has no shared external logger.
    UnknownExternalLogger,
    /// Shutdown event arrived out of order.
    InvalidShutdownTransition,
    /// Archive count cannot fit the target platform.
    RetentionCountTooLarge,
}

impl Display for LoggingError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyExternalCommand => formatter.write_str("external logger command is empty"),
            Self::EmptyFileSelection => formatter.write_str("local log selection is empty"),
            Self::UnknownPipeline => formatter.write_str("unknown logging pipeline"),
            Self::UnknownStage => formatter.write_str("unknown logger stage"),
            Self::UnknownExternalLogger => formatter.write_str("external logger is not configured"),
            Self::InvalidShutdownTransition => {
                formatter.write_str("invalid logging shutdown transition")
            }
            Self::RetentionCountTooLarge => {
                formatter.write_str("logging retention count is too large")
            }
        }
    }
}

impl Error for LoggingError {}
