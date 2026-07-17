//! Live logging pipeline state and health summarization.
//!
//! Runtime values allocate stable pipe identities before process launch and
//! then update only generation and lifecycle state. Private route structures
//! keep pipe ownership local while public queries expose immutable status and
//! aggregate health without leaking duplicate type paths.

use super::{FileRoutePlan, LoggerStage, LoggingError, LoggingPlan, OutputStream};
use crate::supervisor::SupervisorState;

/// Durable identity for stable pipe endpoints, independent of child PIDs.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PipeId(u64);

impl PipeId {
    /// Numeric identity exposed in diagnostics.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// Health summarized across every separately supervised logger stage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PipelineHealth {
    /// Every stage is ready to consume bytes.
    Ready,
    /// At least one stage is starting or stopped.
    Starting,
    /// At least one stage is waiting in restart backoff.
    Backoff,
    /// At least one stage exhausted its configured restart policy.
    Failed,
}

/// Runtime status for one logger process.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggerStageStatus {
    /// Stage definition.
    pub stage: LoggerStage,
    /// Stage-local logical generation.
    pub generation: u64,
    /// Observed child lifecycle.
    pub state: SupervisorState,
}

/// Runtime ownership of stable pipes and restartable logger stages.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoggingRuntime {
    local_files: Vec<FileRouteRuntime>,
    logger: Option<SharedLoggerRuntime>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct FileRouteRuntime {
    plan: FileRoutePlan,
    pipe: PipeId,
    stage: LoggerStageStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SharedLoggerRuntime {
    pipe: PipeId,
    stage: LoggerStageStatus,
}

impl LoggingRuntime {
    /// Allocate durable pipe identities before starting any logger or service.
    #[must_use]
    pub fn new(plan: LoggingPlan) -> Self {
        let logger_position = plan.local_files.len();
        let local_files = plan
            .local_files
            .into_iter()
            .enumerate()
            .map(|(position, plan)| {
                let stage = LoggerStageStatus {
                    stage: LoggerStage::FileAdapter(plan.file.clone()),
                    generation: 0,
                    state: SupervisorState::Down,
                };
                FileRouteRuntime {
                    pipe: PipeId(
                        u64::try_from(position)
                            .unwrap_or(u64::MAX)
                            .saturating_add(1),
                    ),
                    plan,
                    stage,
                }
            })
            .collect();
        let logger = plan.logger.map(|command| SharedLoggerRuntime {
            pipe: PipeId(
                u64::try_from(logger_position)
                    .unwrap_or(u64::MAX)
                    .saturating_add(1),
            ),
            stage: LoggerStageStatus {
                stage: LoggerStage::External(command),
                generation: 0,
                state: SupervisorState::Down,
            },
        });
        Self {
            local_files,
            logger,
        }
    }

    /// Stable service-input pipe identity for an output stream.
    #[must_use]
    pub fn pipe(&self, stream: OutputStream) -> Option<PipeId> {
        self.local_files
            .iter()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .map(|route| route.pipe)
            .or_else(|| self.logger.as_ref().map(|logger| logger.pipe))
    }

    /// Stable shared pipe feeding the one external logger.
    #[must_use]
    pub fn logger_pipe(&self) -> Option<PipeId> {
        self.logger.as_ref().map(|logger| logger.pipe)
    }

    /// Replace one file-adapter generation/state without replacing its pipe.
    ///
    /// # Errors
    ///
    /// Returns an error for an unknown stream or a nonzero stage index.
    pub fn update_stage(
        &mut self,
        stream: OutputStream,
        stage_index: usize,
        generation: u64,
        lifecycle_state: SupervisorState,
    ) -> Result<(), LoggingError> {
        if stage_index != 0 {
            return Err(LoggingError::UnknownStage);
        }
        let route = self
            .local_files
            .iter_mut()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .ok_or(LoggingError::UnknownPipeline)?;
        route.stage.generation = generation;
        route.stage.state = lifecycle_state;
        Ok(())
    }

    /// Replace the shared external logger state without replacing its pipe.
    ///
    /// # Errors
    ///
    /// Returns an error when no external logger is configured.
    pub fn update_logger(
        &mut self,
        generation: u64,
        lifecycle_state: SupervisorState,
    ) -> Result<(), LoggingError> {
        let logger = self
            .logger
            .as_mut()
            .ok_or(LoggingError::UnknownExternalLogger)?;
        logger.stage.generation = generation;
        logger.stage.state = lifecycle_state;
        Ok(())
    }

    /// Aggregate health for one stream's local route and shared logger.
    #[must_use]
    pub fn health(&self, stream: OutputStream) -> Option<PipelineHealth> {
        let local = self
            .local_files
            .iter()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .map(|route| &route.stage);
        let logger = self.logger.as_ref().map(|logger| &logger.stage);
        let stages: Vec<&LoggerStageStatus> = local.into_iter().chain(logger).collect();
        (!stages.is_empty()).then(|| summarize_health(&stages))
    }

    /// Immutable stage statuses affecting one stream.
    #[must_use]
    pub fn stages(&self, stream: OutputStream) -> Vec<&LoggerStageStatus> {
        let local = self
            .local_files
            .iter()
            .find(|route| route_matches_stream(route.plan.stream, stream))
            .map(|route| &route.stage);
        local
            .into_iter()
            .chain(self.logger.as_ref().map(|logger| &logger.stage))
            .collect()
    }
}

fn route_matches_stream(route: OutputStream, stream: OutputStream) -> bool {
    route == stream
        || matches!(
            (route, stream),
            (
                OutputStream::Combined,
                OutputStream::Stdout | OutputStream::Stderr
            )
        )
}

fn summarize_health(stages: &[&LoggerStageStatus]) -> PipelineHealth {
    if stages
        .iter()
        .any(|stage| matches!(stage.state, SupervisorState::Failed(_)))
    {
        PipelineHealth::Failed
    } else if stages
        .iter()
        .any(|stage| matches!(stage.state, SupervisorState::Backoff { .. }))
    {
        PipelineHealth::Backoff
    } else if stages
        .iter()
        .all(|stage| matches!(stage.state, SupervisorState::Ready(_)))
    {
        PipelineHealth::Ready
    } else {
        PipelineHealth::Starting
    }
}
