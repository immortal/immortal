//! Typed observation delivered by the broker to its supervisor.
//!
//! [`ProcessBrokerEvent`] is the supervisor-facing projection of the wire
//! [`BrokerEvent`]: its `From` conversion resolves each generation against
//! [`BrokerTaskId`] so auxiliary hook and logger tasks surface their own
//! `Task*` variants instead of the ordinary service ones, and maps the wire
//! [`BrokerReadinessFailure`] onto the public [`ReadinessFailure`].

use crate::supervisor::Generation;

use super::types::{BrokerTaskId, ReadinessFailure};
use super::{
    BrokerEvent, BrokerReadinessFailure, ChildEvent, ProcessGroupId, ProcessId, SpawnFailure,
    SpawnStage,
};

/// Typed observation delivered by the broker to its supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessBrokerEvent {
    Ready,
    Started {
        generation: Generation,
        process: ProcessId,
        group: ProcessGroupId,
    },
    SpawnFailed {
        generation: Generation,
        stage: SpawnStage,
        failure: SpawnFailure,
        os_error: Option<i32>,
        cleanup_pending: Option<ProcessId>,
    },
    Child {
        generation: Generation,
        event: ChildEvent,
    },
    TaskStarted {
        task: BrokerTaskId,
        process: ProcessId,
        group: ProcessGroupId,
    },
    TaskSpawnFailed {
        task: BrokerTaskId,
        stage: SpawnStage,
        failure: SpawnFailure,
        os_error: Option<i32>,
        cleanup_pending: Option<ProcessId>,
    },
    TaskChild {
        task: BrokerTaskId,
        event: ChildEvent,
    },
    SignalDelivered {
        generation: Generation,
    },
    SignalFailed {
        generation: Generation,
        os_error: Option<i32>,
    },
    TaskSignalDelivered {
        task: BrokerTaskId,
    },
    TaskSignalFailed {
        task: BrokerTaskId,
        os_error: Option<i32>,
    },
    Detached {
        generation: Generation,
    },
    DetachFailed {
        generation: Generation,
    },
    GenerationReady {
        generation: Generation,
    },
    ReadinessFailed {
        generation: Generation,
        failure: ReadinessFailure,
    },
    LifetimeClosed {
        generation: Generation,
    },
    LifetimeFailed {
        generation: Generation,
    },
    /// The fail-closed helper disappeared, so the broker killed this generation group.
    ContainmentFailed {
        generation: Generation,
    },
    /// The fail-closed helper disappeared, so the broker killed this task group.
    TaskContainmentFailed {
        task: BrokerTaskId,
    },
    LoggerInputsClosed,
    ShutdownComplete,
    ShutdownFailed {
        os_error: Option<i32>,
    },
}

impl From<BrokerEvent> for ProcessBrokerEvent {
    fn from(event: BrokerEvent) -> Self {
        match event {
            BrokerEvent::Ready => Self::Ready,
            BrokerEvent::Started {
                generation,
                process,
                group,
            } => BrokerTaskId::from_generation(generation).map_or(
                Self::Started {
                    generation,
                    process,
                    group,
                },
                |task| Self::TaskStarted {
                    task,
                    process,
                    group,
                },
            ),
            BrokerEvent::SpawnFailed {
                generation,
                stage,
                failure,
                os_error,
                cleanup_pending,
            } => BrokerTaskId::from_generation(generation).map_or(
                Self::SpawnFailed {
                    generation,
                    stage,
                    failure,
                    os_error,
                    cleanup_pending,
                },
                |task| Self::TaskSpawnFailed {
                    task,
                    stage,
                    failure,
                    os_error,
                    cleanup_pending,
                },
            ),
            BrokerEvent::Child { generation, event } => BrokerTaskId::from_generation(generation)
                .map_or(Self::Child { generation, event }, |task| Self::TaskChild {
                    task,
                    event,
                }),
            BrokerEvent::SignalDelivered { generation } => {
                BrokerTaskId::from_generation(generation)
                    .map_or(Self::SignalDelivered { generation }, |task| {
                        Self::TaskSignalDelivered { task }
                    })
            }
            BrokerEvent::SignalFailed {
                generation,
                os_error,
            } => BrokerTaskId::from_generation(generation).map_or(
                Self::SignalFailed {
                    generation,
                    os_error,
                },
                |task| Self::TaskSignalFailed { task, os_error },
            ),
            BrokerEvent::Detached { generation } => Self::Detached { generation },
            BrokerEvent::DetachFailed { generation } => Self::DetachFailed { generation },
            BrokerEvent::GenerationReady { generation } => Self::GenerationReady { generation },
            BrokerEvent::ReadinessFailed {
                generation,
                failure,
            } => Self::ReadinessFailed {
                generation,
                failure: match failure {
                    BrokerReadinessFailure::Timeout => ReadinessFailure::Timeout,
                    BrokerReadinessFailure::Descriptor => ReadinessFailure::Descriptor,
                    BrokerReadinessFailure::InvalidToken => ReadinessFailure::InvalidToken,
                },
            },
            BrokerEvent::LifetimeClosed { generation } => Self::LifetimeClosed { generation },
            BrokerEvent::LifetimeFailed { generation } => Self::LifetimeFailed { generation },
            BrokerEvent::ContainmentFailed { generation } => {
                BrokerTaskId::from_generation(generation)
                    .map_or(Self::ContainmentFailed { generation }, |task| {
                        Self::TaskContainmentFailed { task }
                    })
            }
            BrokerEvent::LoggerInputsClosed => Self::LoggerInputsClosed,
            BrokerEvent::ShutdownComplete => Self::ShutdownComplete,
            BrokerEvent::ShutdownFailed { os_error } => Self::ShutdownFailed { os_error },
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;

    use super::*;

    #[test]
    fn containment_event_preserves_service_and_task_identity() -> Result<(), Box<dyn Error>> {
        let generation = Generation::FIRST;
        assert_eq!(
            ProcessBrokerEvent::from(BrokerEvent::ContainmentFailed { generation }),
            ProcessBrokerEvent::ContainmentFailed { generation }
        );

        let task = BrokerTaskId::new(7)
            .ok_or_else(|| io::Error::other("test task identifier must be valid"))?;
        let task_generation = task
            .generation()
            .ok_or_else(|| io::Error::other("test task must map to a generation"))?;
        assert_eq!(
            ProcessBrokerEvent::from(BrokerEvent::ContainmentFailed {
                generation: task_generation,
            }),
            ProcessBrokerEvent::TaskContainmentFailed { task }
        );
        Ok(())
    }
}
