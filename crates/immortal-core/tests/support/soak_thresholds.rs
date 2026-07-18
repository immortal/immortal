//! Bounded-growth thresholds for the resource-soak contract.
//!
//! These pure predicates decide whether one sampled broker metric stayed within
//! its generous absolute tolerance across a soak run. They are separated from
//! the broker-driving contract so the pass and fail decisions can be exercised
//! directly, without forking a live broker, and reused by any later soak stage
//! that wraps the full supervisor. Each predicate is deliberately permissive:
//! it only reports a regression when growth clearly exceeds steady-state jitter,
//! so a green run is strong evidence and a red run is worth investigating.

use std::time::Duration;

/// Absolute resident-memory growth tolerated over a whole run, in kibibytes.
pub const RSS_TOLERANCE_KIB: u64 = 4096;
/// Absolute open-descriptor growth tolerated over a whole run.
pub const FD_TOLERANCE: u64 = 8;
/// Largest at-rest child count tolerated between restart cycles.
pub const CHILD_CEILING: u64 = 4;
/// Slowest single restart round trip tolerated before a stall is reported.
pub const LATENCY_CEILING: Duration = Duration::from_secs(5);

/// Whether resident memory stayed within tolerance of its baseline sample.
///
/// A run without a memory baseline (sampling unavailable on the platform)
/// cannot demonstrate growth, so it is treated as within bounds rather than
/// failing a diagnostic that could not observe the resource.
#[must_use]
pub fn resident_within(baseline: Option<u64>, value: u64) -> bool {
    baseline.is_none_or(|base| value <= base.saturating_add(RSS_TOLERANCE_KIB))
}

/// Whether the open-descriptor count stayed within tolerance of its baseline.
///
/// Like [`resident_within`], an absent baseline is treated as within bounds.
#[must_use]
pub fn descriptors_within(baseline: Option<u64>, value: u64) -> bool {
    baseline.is_none_or(|base| value <= base.saturating_add(FD_TOLERANCE))
}

/// Whether the at-rest child count stayed at or below the ceiling.
///
/// Children are counted while the broker is idle between cycles, so anything
/// above the ceiling means a terminated child was not reaped.
#[must_use]
pub fn children_within(value: u64) -> bool {
    value <= CHILD_CEILING
}

/// Whether the peak restart round trip stayed at or below the stall ceiling.
#[must_use]
pub fn latency_within(latency: Duration) -> bool {
    latency <= LATENCY_CEILING
}
