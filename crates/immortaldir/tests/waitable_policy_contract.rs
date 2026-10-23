//! Real immortaldir reconciliation under inherited ignored SIGCHLD.
//!
//! The launcher fixture uses the real core daemon executor and a finite service.
//! GNU interposition covers policy-reset failure before any fork or runtime.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[path = "waitable_policy_contract/linux.rs"]
mod linux;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn main() {}
