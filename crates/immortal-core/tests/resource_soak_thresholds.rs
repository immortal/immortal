//! Unit coverage for the resource-soak growth thresholds.
//!
//! The soak contract itself forks a live broker and can only observe a healthy
//! run on trustworthy hardware, so these tests pin the pass and fail decisions
//! directly. Each threshold is proven to accept steady-state growth up to its
//! tolerance and to reject the first value beyond it, which is the behavior a
//! real leak or stall would trip.

#[path = "support/soak_thresholds.rs"]
mod soak_thresholds;

use std::time::Duration;

use soak_thresholds::{
    CHILD_CEILING, FD_TOLERANCE, LATENCY_CEILING, RSS_TOLERANCE_KIB, children_within,
    descriptors_within, latency_within, resident_within,
};

#[test]
fn resident_within_accepts_growth_up_to_tolerance() {
    let baseline = 2668;
    assert!(resident_within(
        Some(baseline),
        baseline + RSS_TOLERANCE_KIB
    ));
}

#[test]
fn resident_within_rejects_growth_past_tolerance() {
    let baseline = 2668;
    assert!(!resident_within(
        Some(baseline),
        baseline + RSS_TOLERANCE_KIB + 1
    ));
}

#[test]
fn resident_within_without_baseline_cannot_regress() {
    assert!(resident_within(None, u64::MAX));
}

#[test]
fn descriptors_within_accepts_growth_up_to_tolerance() {
    assert!(descriptors_within(Some(10), 10 + FD_TOLERANCE));
}

#[test]
fn descriptors_within_rejects_a_leak() {
    assert!(!descriptors_within(Some(10), 10 + FD_TOLERANCE + 1));
}

#[test]
fn descriptors_within_without_baseline_cannot_regress() {
    assert!(descriptors_within(None, u64::MAX));
}

#[test]
fn children_within_accepts_the_ceiling() {
    assert!(children_within(CHILD_CEILING));
}

#[test]
fn children_within_rejects_retained_children() {
    assert!(!children_within(CHILD_CEILING + 1));
}

#[test]
fn latency_within_accepts_the_ceiling() {
    assert!(latency_within(LATENCY_CEILING));
}

#[test]
fn latency_within_rejects_a_stall() {
    assert!(!latency_within(LATENCY_CEILING + Duration::from_millis(1)));
}
