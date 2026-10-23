//! Inherited SIGCHLD policy through exec of the installed CLI code path.
//!
//! GNU/Linux preload forces the broker's normal exit at the cleanup deadline.
//! Other platforms skip interposition; portable API cases live in fork's
//! harness-free `waitable_children` contract.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[path = "waitable_policy_contract/linux.rs"]
mod linux;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn main() {}
