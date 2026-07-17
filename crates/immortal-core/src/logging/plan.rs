//! Logging graph planning from validated service configuration.
//!
//! This child owns the pure description layer before any pipes, children, or
//! descriptors exist. It preserves route ordering and validation precedence so
//! later runtime setup can allocate durable resources without reinterpreting
//! configuration policy.

use super::LoggingError;
use crate::config::{FileLogConfig, FileLogRoutes, LoggingConfig};

/// Child output stream attached to one pipeline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OutputStream {
    /// Standard output only.
    Stdout,
    /// Standard error only.
    Stderr,
    /// Standard output and error share one pipe.
    Combined,
}

/// Backpressure behavior between a service and logger chain.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BackpressurePolicy {
    /// Block producers through normal kernel-pipe pressure; never drop silently.
    LosslessBlock,
}

/// One separately supervised process in a logger chain.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoggerStage {
    /// Small replaceable `immortallog` compatibility process.
    FileAdapter(FileLogConfig),
    /// Operator-selected external logger argv.
    External(Vec<String>),
}

/// One local-file route attached to an exact child stream selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileRoutePlan {
    /// Service descriptors connected to this adapter.
    pub stream: OutputStream,
    /// Local destination and rotation policy.
    pub file: FileLogConfig,
    /// Loss policy between processes.
    pub backpressure: BackpressurePolicy,
}

/// Complete local-file and centralized-logger graph for one service.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LoggingPlan {
    /// Zero, one, or two independently supervised local file adapters.
    pub local_files: Vec<FileRoutePlan>,
    /// At most one external logger receiving merged stdout and stderr.
    pub logger: Option<Vec<String>>,
}

impl LoggingPlan {
    /// Normalize output configuration into local routes and one shared sink.
    ///
    /// Local stream selection never filters the external logger. When both are
    /// configured, file-adapter passthrough writers converge on one kernel pipe
    /// owned by the broker.
    ///
    /// # Errors
    ///
    /// Returns an error for an empty external command or empty selected route.
    pub fn from_config(config: &LoggingConfig) -> Result<Self, LoggingError> {
        if config
            .logger
            .as_ref()
            .is_some_and(|command| command.first().is_none_or(String::is_empty))
        {
            return Err(LoggingError::EmptyExternalCommand);
        }

        let mut local_files = Vec::new();
        match &config.files {
            Some(FileLogRoutes::Combined(file)) => local_files.push(FileRoutePlan {
                stream: OutputStream::Combined,
                file: file.clone(),
                backpressure: BackpressurePolicy::LosslessBlock,
            }),
            Some(FileLogRoutes::Selected { stdout, stderr }) => {
                if stdout.is_none() && stderr.is_none() {
                    return Err(LoggingError::EmptyFileSelection);
                }
                if let Some(file) = stdout {
                    local_files.push(FileRoutePlan {
                        stream: OutputStream::Stdout,
                        file: file.clone(),
                        backpressure: BackpressurePolicy::LosslessBlock,
                    });
                }
                if let Some(file) = stderr {
                    local_files.push(FileRoutePlan {
                        stream: OutputStream::Stderr,
                        file: file.clone(),
                        backpressure: BackpressurePolicy::LosslessBlock,
                    });
                }
            }
            None => {}
        }
        Ok(Self {
            local_files,
            logger: config.logger.clone(),
        })
    }
}
