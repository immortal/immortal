//! Broker-owned lifecycle state shared across the event loop, dispatch, spawn,
//! and reap boundaries.
//!
//! Every type here is a plain data holder destructured or mutated by its
//! sibling modules rather than exposing its own behavior: [`BrokerGeneration`]
//! and [`BrokerOwnedProcess`] track which processes the broker must reap and
//! why, [`BrokerRuntimeState`] is the single event-loop-owned aggregate, and
//! [`ReadinessObservation`]/[`LifetimeObservation`] carry results back from
//! detached readiness/lifetime-wait tasks through their bounded channels.

use std::collections::BTreeMap;
use std::io;

use tokio::sync::mpsc;

use crate::readiness::ReadinessError;
use crate::supervisor::Generation;

use super::logging::BrokerLogging;
use super::types::BrokerLifetimePlan;
use super::{ProcessGroupGuard, ProcessGroupId, ProcessId, SpawnedProcess};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LifetimeState {
    Foreground,
    Tracking,
    Closed,
    Failed,
}

pub(super) struct BrokerGeneration {
    pub(super) child: Option<SpawnedProcess>,
    pub(super) guard: Option<ProcessGroupGuard>,
    pub(super) group: ProcessGroupId,
    pub(super) lifetime: LifetimeState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum BrokerOwnedProcess {
    Generation(Generation),
    Guard(Generation),
}

pub(super) struct BrokerRuntimeState {
    pub(super) generations: BTreeMap<Generation, BrokerGeneration>,
    pub(super) logging: BrokerLogging,
    pub(super) lifetime_cleanup: Option<BrokerLifetimePlan>,
    pub(super) lifetime_sender: mpsc::Sender<LifetimeObservation>,
    pub(super) processes: BTreeMap<ProcessId, BrokerOwnedProcess>,
    pub(super) readiness_sender: mpsc::Sender<ReadinessObservation>,
}

pub(super) struct ReadinessObservation {
    pub(super) generation: Generation,
    pub(super) result: Result<(), ReadinessError>,
}

pub(super) struct LifetimeObservation {
    pub(super) generation: Generation,
    pub(super) result: io::Result<()>,
}

pub(super) struct BrokerSpawnState<'a> {
    pub(super) generations: &'a mut BTreeMap<Generation, BrokerGeneration>,
    pub(super) lifetime_sender: &'a mpsc::Sender<LifetimeObservation>,
    pub(super) processes: &'a mut BTreeMap<ProcessId, BrokerOwnedProcess>,
    pub(super) readiness_sender: &'a mpsc::Sender<ReadinessObservation>,
    pub(super) logging: &'a BrokerLogging,
}
