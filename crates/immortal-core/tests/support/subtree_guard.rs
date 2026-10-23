//! Final, signal-free collection after all exact-child and release guards drop.
//!
//! Declare this guard before fixture owners. It alone drains arbitrary adopted
//! children during error unwinding, after no guard can signal a consumed PID.
//! Success paths still use explicit residual assertions before this fallback.

use std::{
    thread,
    time::{Duration, Instant},
};

use immortal_core::process::reap_any_event;

/// Reap orphaned descendants only after every signal-capable owner has retired.
pub struct SubtreeGuard;

impl Drop for SubtreeGuard {
    fn drop(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            match reap_any_event() {
                Ok(Some(_)) => {}
                Ok(None) => thread::sleep(Duration::from_millis(5)),
                Err(_) => return,
            }
        }
    }
}
