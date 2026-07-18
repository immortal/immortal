//! Subreaper acquisition: reparent and reap descendants that escape the subtree.
//!
//! A supervised service may fork grandchildren and exit — or a descendant may
//! deliberately `setsid` out of its owned process group — before those
//! descendants terminate. Without intervention the kernel reparents such
//! orphans to init (PID 1), where the supervisor can neither reap them nor
//! account for them. Registering the broker as a *subreaper* makes the kernel
//! reparent every orphaned descendant of the supervised subtree to the broker
//! instead, so the existing [`super::reap_any_event`] drain observes and reaps
//! them exactly like a direct child, preventing leaked zombies against init.
//!
//! This module is the crate's only boundary onto the `fork` subreaper
//! primitive, and it exists because Immortal's supported platforms disagree on
//! availability: Linux and FreeBSD implement subreaping, macOS does not. Rather
//! than leak that split to callers as a raw [`io::ErrorKind::Unsupported`],
//! every entry point folds "this platform or running kernel cannot subreap"
//! into a benign result — [`SubreaperStatus::Unsupported`], `false`, or a
//! successful no-op release — so the broker degrades to init-reparenting
//! behavior on macOS instead of aborting. Only a genuine operating-system
//! failure (for example a permission error) still surfaces as `Err`.
//!
//! Acquisition is process-wide, idempotent, not inherited across `fork`, and
//! preserved across `exec`; it affects only descendants orphaned after the
//! call. Subreaping reaps escaped descendants but never enumerates them or
//! signals a still-running one: terminating a live process that left its owned
//! group stays the responsibility of process-group teardown, not this module.

use std::io;

/// Result of requesting the subreaper role for the current process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SubreaperStatus {
    /// The current process now reaps orphaned descendants of its subtree.
    Acquired,
    /// The platform or running kernel does not support subreaping, so orphans
    /// reparent to init and cannot be reaped here. This is an expected,
    /// non-fatal condition on a supported platform without the primitive
    /// (currently macOS), not an operating-system failure.
    Unsupported,
}

impl SubreaperStatus {
    /// Whether the current process now holds the subreaper role.
    #[must_use]
    pub const fn is_acquired(self) -> bool {
        matches!(self, Self::Acquired)
    }
}

/// Register the current process as the subreaper for its descendant subtree.
///
/// Idempotent and process-wide; it only affects descendants orphaned after the
/// call. Missing platform support yields [`SubreaperStatus::Unsupported`] so the
/// caller can degrade rather than treating macOS as a fault.
///
/// # Errors
///
/// Returns the operating-system error when acquisition fails for any reason
/// other than missing subreaper support.
pub fn acquire_subreaper() -> io::Result<SubreaperStatus> {
    tolerate_unsupported(
        fork::acquire_subreaper().map(|()| SubreaperStatus::Acquired),
        SubreaperStatus::Unsupported,
    )
}

/// Report whether the current process explicitly holds the subreaper role.
///
/// Reflects an explicitly acquired role rather than the implicit reaping duty of
/// PID 1. A platform or kernel without subreaper support reports `false`.
///
/// # Errors
///
/// Returns the operating-system error when the query fails for any reason other
/// than missing subreaper support.
pub fn is_subreaper() -> io::Result<bool> {
    tolerate_unsupported(fork::is_subreaper(), false)
}

/// Relinquish a previously acquired subreaper role.
///
/// Idempotent: releasing when the role is not held, or on a platform without
/// subreaper support, succeeds without effect. Future orphans then follow the
/// platform's normal reparenting rules.
///
/// # Errors
///
/// Returns the operating-system error when release fails for any reason other
/// than missing subreaper support.
pub fn release_subreaper() -> io::Result<()> {
    tolerate_unsupported(fork::release_subreaper(), ())
}

/// Fold "subreaping is unavailable on this platform or kernel" into the benign
/// `unsupported` value, leaving every other operating-system failure as `Err`.
///
/// `fork` reports absent support as [`io::ErrorKind::Unsupported`] — at compile
/// time on macOS and at runtime on a kernel too old for the primitive — and
/// Immortal treats both as an expected, non-fatal condition.
fn tolerate_unsupported<T>(result: io::Result<T>, unsupported: T) -> io::Result<T> {
    match result {
        Ok(value) => Ok(value),
        Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(unsupported),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{SubreaperStatus, tolerate_unsupported};

    #[test]
    fn tolerate_unsupported_passes_success_through() -> io::Result<()> {
        assert_eq!(tolerate_unsupported(Ok(7_u8), 0)?, 7);
        Ok(())
    }

    #[test]
    fn tolerate_unsupported_maps_unsupported_to_fallback() -> io::Result<()> {
        let result: io::Result<u8> = Err(io::Error::from(io::ErrorKind::Unsupported));
        assert_eq!(tolerate_unsupported(result, 42)?, 42);
        Ok(())
    }

    #[test]
    fn tolerate_unsupported_propagates_other_errors() {
        let result: io::Result<u8> = Err(io::Error::from(io::ErrorKind::PermissionDenied));
        assert_eq!(
            tolerate_unsupported(result, 0)
                .err()
                .map(|error| error.kind()),
            Some(io::ErrorKind::PermissionDenied),
        );
    }

    #[test]
    fn subreaper_status_reports_acquisition() {
        assert!(SubreaperStatus::Acquired.is_acquired());
        assert!(!SubreaperStatus::Unsupported.is_acquired());
    }
}
