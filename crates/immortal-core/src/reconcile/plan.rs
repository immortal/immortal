//! Semantic desired-state action planning.
//!
//! Planning compares normalized service configurations rather than file metadata,
//! preserving caller-controlled deletion policy. Invalid replacement files can be
//! merged with the previous last-known-good map without implying a stop.

use std::collections::BTreeMap;

use super::scan::ScanResult;
use crate::config::ServiceConfig;

/// Desired-state change computed from two valid semantic snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileAction {
    /// Definition became enabled and has no current supervisor.
    Start,
    /// Valid normalized configuration changed.
    Restart,
    /// Definition is disabled or was stably removed.
    Stop,
    /// Valid desired and current state are already equivalent.
    Keep,
}

/// Compute semantic desired-state action without relying on mtimes.
#[must_use]
pub fn compare(
    previous: Option<&ServiceConfig>,
    desired: Option<&ServiceConfig>,
) -> ReconcileAction {
    match (previous, desired) {
        (None, Some(config)) if config.enabled => ReconcileAction::Start,
        (Some(_), None) => ReconcileAction::Stop,
        (None, Some(_) | None) => ReconcileAction::Keep,
        (Some(old), Some(new)) if old == new => ReconcileAction::Keep,
        (Some(old), Some(new)) if old.enabled && !new.enabled => ReconcileAction::Stop,
        (Some(old), Some(new)) if !old.enabled && new.enabled => ReconcileAction::Start,
        (Some(_), Some(new)) if !new.enabled => ReconcileAction::Keep,
        (Some(_), Some(_)) => ReconcileAction::Restart,
    }
}

/// Merge a scan into last-known-good desired state.
///
/// Names with invalid replacement files retain their previous valid value.
/// Valid definitions replace prior values. Stable deletion handling remains a
/// caller policy because a single partial scan must not imply deletion.
#[must_use]
pub fn retain_last_known_good(
    previous: &BTreeMap<String, ServiceConfig>,
    scan: &ScanResult,
) -> BTreeMap<String, ServiceConfig> {
    let mut desired = previous.clone();
    desired.extend(
        scan.definitions
            .iter()
            .map(|(name, definition)| (name.clone(), definition.config.clone())),
    );
    desired
}
