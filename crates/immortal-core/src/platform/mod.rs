//! Operating-system integration points supported by immortal.

#[cfg(target_os = "freebsd")]
pub mod freebsd;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(not(any(target_os = "freebsd", target_os = "linux", target_os = "macos")))]
compile_error!("immortal currently supports Linux, macOS, and FreeBSD");
