//! In-memory desired-state tracking across authoritative directory scans.
//!
//! The tracker owns last-known-good configuration state and a bounded absence
//! counter per service. Invalid replacements count as present, incomplete scans
//! never confirm deletion, and confirmed deletions remain replayable until the
//! caller acknowledges successful supervisor and applied-state cleanup.

use std::{
    collections::{BTreeMap, BTreeSet},
    num::NonZeroUsize,
};

use super::{
    limits::DEFAULT_DELETION_CONFIRMATIONS,
    plan::{ReconcileAction, compare},
    scan::{ScanProblemKind, ScanResult, definition_name, is_candidate},
};
use crate::config::ServiceConfig;

/// Persistent desired-state view built from authoritative directory scans.
///
/// Valid definitions replace older values immediately. Invalid replacements
/// retain their last-known-good value and count as present. A missing file must
/// remain absent for the configured number of consecutive scans before its
/// desired state is removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredStateTracker {
    pub(in crate::reconcile) desired: BTreeMap<String, ServiceConfig>,
    pub(in crate::reconcile) absent_scans: BTreeMap<String, usize>,
    pub(in crate::reconcile) deletion_confirmations: NonZeroUsize,
}

impl Default for DesiredStateTracker {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(DEFAULT_DELETION_CONFIRMATIONS).unwrap_or(NonZeroUsize::MIN))
    }
}

impl DesiredStateTracker {
    /// Construct a tracker with an explicit stable-deletion threshold.
    #[must_use]
    pub const fn new(deletion_confirmations: NonZeroUsize) -> Self {
        Self {
            desired: BTreeMap::new(),
            absent_scans: BTreeMap::new(),
            deletion_confirmations,
        }
    }

    /// Current last-known-good desired configurations.
    #[must_use]
    pub const fn desired(&self) -> &BTreeMap<String, ServiceConfig> {
        &self.desired
    }

    /// Forget a confirmed deletion only after the supervisor and applied state
    /// have been removed successfully.
    pub fn acknowledge_deletion(&mut self, name: &str) {
        if !self.desired.contains_key(name) {
            self.absent_scans.remove(name);
        }
    }

    /// Apply one complete scan and return one deterministic action per known name.
    ///
    /// Reapplying an identical scan yields `Keep`. A confirmed deletion yields
    /// `Stop` once in memory; a persisted unacknowledged deletion is replayed
    /// after restart so cleanup cannot be lost.
    pub fn apply(&mut self, scan: &ScanResult) -> BTreeMap<String, ReconcileAction> {
        let problem_names: BTreeSet<String> = scan
            .problems
            .iter()
            .filter_map(|problem| {
                is_candidate(&problem.path)
                    .then(|| definition_name(&problem.path))
                    .flatten()
            })
            .collect();
        let scan_incomplete = scan.problems.iter().any(|problem| {
            matches!(&problem.kind, ScanProblemKind::Io(_)) && !is_candidate(&problem.path)
        });
        let mut actions = BTreeMap::new();

        for (name, definition) in &scan.definitions {
            let action = compare(self.desired.get(name), Some(&definition.config));
            self.desired.insert(name.clone(), definition.config.clone());
            self.absent_scans.remove(name);
            actions.insert(name.clone(), action);
        }

        for name in &problem_names {
            self.absent_scans.remove(name);
            if self.desired.contains_key(name) {
                actions.entry(name.clone()).or_insert(ReconcileAction::Keep);
            }
        }

        let missing: Vec<String> = self
            .desired
            .keys()
            .filter(|name| !scan.definitions.contains_key(*name) && !problem_names.contains(*name))
            .cloned()
            .collect();
        for name in missing {
            if scan_incomplete {
                actions.insert(name, ReconcileAction::Keep);
                continue;
            }
            let count = self.absent_scans.entry(name.clone()).or_default();
            *count = count.saturating_add(1);
            if *count >= self.deletion_confirmations.get() {
                *count = self.deletion_confirmations.get();
                self.desired.remove(&name);
                actions.insert(name, ReconcileAction::Stop);
            } else {
                actions.insert(name, ReconcileAction::Keep);
            }
        }
        actions
    }
}
