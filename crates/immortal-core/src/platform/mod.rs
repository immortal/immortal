//! Operating-system integration points supported by immortal.

mod account;

pub(crate) use account::resolve as resolve_account;

#[cfg(target_os = "freebsd")]
pub mod freebsd;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "freebsd")]
pub(crate) use freebsd::file_identity;
#[cfg(target_os = "linux")]
pub(crate) use linux::file_identity;
#[cfg(target_os = "macos")]
pub(crate) use macos::file_identity;

/// Stable identity of one filesystem object within a mounted filesystem.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileIdentity {
    device: u64,
    inode: u64,
}

impl FileIdentity {
    pub(crate) const fn new(device: u64, inode: u64) -> Self {
        Self { device, inode }
    }
}

#[cfg(not(any(target_os = "freebsd", target_os = "linux", target_os = "macos")))]
compile_error!("immortal currently supports Linux, macOS, and FreeBSD");
