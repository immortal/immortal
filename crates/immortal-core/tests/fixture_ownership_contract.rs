//! Unwinding must not steal the broker's terminal event or signal an unpinned ID.
//!
//! GNU/Linux interposition records and suppresses unsafe deliveries before they
//! reach the kernel. Real children reproduce an exited broker and surviving
//! member; a second case models group reuse after its leader has been reaped.

#[cfg(all(target_os = "linux", target_env = "gnu"))]
#[path = "fixture_ownership_contract/linux.rs"]
mod linux;

#[cfg(all(target_os = "linux", target_env = "gnu"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    linux::run()
}

#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
fn main() {}
