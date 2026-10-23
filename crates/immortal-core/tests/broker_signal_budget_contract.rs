//! Real-broker signal budgets with eight stopped guard helpers.
//!
//! Linux procfs identifies only this test broker's children. Both failure and
//! success paths resume helpers through pinned ownership. One broker guard caches
//! terminal observations; a final signal-free drain collects adopted descendants. A TERM-ignoring sibling additionally
//! proves that a skipped foreground kill cannot intentionally disarm containment.
//! `--sibling-probe PROGRAM PID_FILE` runs that schedule with an external fixture.
//! `--broker-exit-probe` forces terminal collection before auxiliary resume.
//! Other platforms skip procfs discovery.

#[cfg(target_os = "linux")]
#[path = "support/broker_guard.rs"]
mod broker_guard;
#[cfg(target_os = "linux")]
#[path = "broker_signal_budget_contract/linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
#[path = "support/residual_children.rs"]
mod residual_children;
#[cfg(target_os = "linux")]
#[path = "support/subtree_guard.rs"]
mod subtree_guard;

#[cfg(target_os = "linux")]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let _orphans = subtree_guard::SubtreeGuard;
    linux::run()
}

/// Helper discovery requires Linux procfs; other targets intentionally skip it.
#[cfg(not(target_os = "linux"))]
fn main() {}
