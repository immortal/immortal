//! Durable release ownership installed before any broker or fixture is started.
//!
//! The directory is declared before signal-capable owners. After those owners
//! drop, release every waiter, collect descendants, and observe recorded groups
//! until absent. If absence cannot be proved, retain the release files rather
//! than revoke a handshake a delayed recipient still needs. Probes grant no
//! signal authority. Re-executed fixtures borrow paths and never own this guard.

use std::{
    cell::RefCell,
    fs,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use immortal_core::process::{ProcessGroupId, process_group_exists};

use crate::subtree_guard;

/// Shared path borrows register groups in this single-threaded owner. The small
/// vector allocates only in the test parent; no lock or shared process state is
/// needed. Drop alone drains children, after the broker guards are gone.
pub struct ReleaseDirectory {
    base: PathBuf,
    markers: &'static [&'static str],
    groups: RefCell<Vec<ProcessGroupId>>,
}

impl ReleaseDirectory {
    pub const fn new(base: PathBuf, markers: &'static [&'static str]) -> Self {
        Self {
            base,
            markers,
            groups: RefCell::new(Vec::new()),
        }
    }

    /// Record an observation before any subsequent completion step can fail.
    pub fn record(&self, group: ProcessGroupId) {
        self.groups.borrow_mut().push(group);
    }
}

impl Drop for ReleaseDirectory {
    fn drop(&mut self) {
        for marker in self.markers {
            let _ = fs::write(self.base.join(marker), "release");
        }
        // No signal-capable fixture owner remains. Keep files through collection,
        // including the interval before a delayed member's first release poll.
        drop(subtree_guard::SubtreeGuard);
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            if self
                .groups
                .get_mut()
                .iter()
                .all(|group| process_group_exists(*group).is_ok_and(|present| !present))
            {
                let _ = fs::remove_dir_all(&self.base);
                return;
            }
            if Instant::now() >= deadline {
                eprintln!(
                    "retaining release directory for unfinished fixtures: {}",
                    self.base.display()
                );
                return;
            }
            thread::sleep(Duration::from_millis(5));
        }
    }
}
