//! Local-file routing, shared external-logger planning, and file rotation.
//!
//! Configuration normalizes into strict local routes plus at most one external
//! sink. Combined routes use one file adapter; selected routes keep stdout and
//! stderr independent locally while both may converge on the shared sink.
//! This facade preserves `immortal_core::logging` as the single public path
//! while private children own the cohesive pieces: `plan` describes the desired
//! graph, `runtime` tracks live pipe and stage state, `shutdown` enforces
//! stop/drain ordering, `error` owns typed failures, and `rotation` owns the
//! rotating file adapter and archive catalog.

mod error;
mod plan;
mod rotation;
mod runtime;
mod shutdown;

pub use self::error::LoggingError;
pub use self::plan::{BackpressurePolicy, FileRoutePlan, LoggerStage, LoggingPlan, OutputStream};
pub use self::rotation::{Archive, RotatingFile, RotationPolicy, archives};
pub use self::runtime::{LoggerStageStatus, LoggingRuntime, PipeId, PipelineHealth};
pub use self::shutdown::{LoggingShutdown, LoggingShutdownEffect, LoggingShutdownPhase};

#[cfg(test)]
mod tests;
