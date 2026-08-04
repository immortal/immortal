//! Bounded discovery and desired-state reconciliation for `immortaldir`.
//!
//! A scan reads stable, size-limited definition snapshots and isolates invalid
//! candidates. The desired-state tracker converts complete scans into semantic
//! actions with confirmed deletion, while [`DefinitionSnapshots`] atomically
//! publishes normalized launch/applied state and a bounded deletion ledger below
//! an owner-only runtime root. [`SupervisorLauncher`] moves one pre-Tokio broker
//! client through bounded checked daemon-launch batches; it owns mechanism only,
//! leaving lifecycle policy to the calling reconciliation loop.
//!
//! This module is a thin facade over private children: `limits` owns bounded scan
//! and launch policy, `scan` owns definition discovery and stable reads,
//! `snapshots` owns durable normalized state, `launcher` owns broker-backed
//! supervisor starts, `plan` owns semantic action comparison, `tracker` owns
//! deletion-confirmation state, and `dependency` owns start-wave planning. The
//! facade re-exports the public surface so `immortal_core::reconcile::*` remains
//! the only reachable path for callers.

mod dependency;
mod launcher;
mod limits;
mod plan;
mod scan;
mod snapshots;
#[cfg(test)]
mod tests;
mod tracker;

pub use self::dependency::{DependencyError, DependencyPlan, UnresolvableService, dependency_plan};
pub use self::launcher::{LauncherError, LauncherTaskError, SupervisorLaunch, SupervisorLauncher};
pub use self::limits::{
    DEFAULT_DELETION_CONFIRMATIONS, DEFAULT_MAX_CONCURRENT_LAUNCHES, DEFAULT_MAX_DEFINITIONS,
    LaunchConcurrency, LaunchConcurrencyError, MAX_CONCURRENT_LAUNCHES, ScanLimits,
};
pub use self::plan::{ReconcileAction, compare, retain_last_known_good};
pub use self::scan::{
    Definition, ScanError, ScanProblem, ScanProblemKind, ScanResult,
    canonical_definitions_directory, scan_directory,
};
pub use self::snapshots::DefinitionSnapshots;
pub use self::tracker::DesiredStateTracker;

#[cfg(test)]
use self::scan::metadata_changed;
#[cfg(test)]
use self::snapshots::{MAX_TRACKER_STATE_BYTES, TRACKER_STATE_FILE};
