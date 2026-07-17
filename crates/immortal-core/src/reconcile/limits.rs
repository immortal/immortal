//! Resource bounds for reconciliation scans and supervisor-launch batches.
//!
//! This module owns the public defaults and checked concurrency wrapper used by
//! directory reconciliation. Validation happens before a launch batch reaches the
//! process broker, so callers cannot accidentally submit an empty or unbounded
//! batch across the daemon boundary.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    num::NonZeroUsize,
};

/// Default maximum number of candidate definitions accepted in one directory.
pub const DEFAULT_MAX_DEFINITIONS: usize = 4096;
/// Consecutive authoritative scans required before a missing definition is removed.
pub const DEFAULT_DELETION_CONFIRMATIONS: usize = 2;
/// Default maximum checked supervisor launches submitted at once.
pub const DEFAULT_MAX_CONCURRENT_LAUNCHES: usize = 8;
/// Hard upper bound for one checked supervisor-launch batch.
pub const MAX_CONCURRENT_LAUNCHES: usize = 64;

/// Validated upper bound for one concurrent supervisor-launch batch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LaunchConcurrency(NonZeroUsize);

impl LaunchConcurrency {
    /// Validate a nonzero launch limit within [`MAX_CONCURRENT_LAUNCHES`].
    ///
    /// # Errors
    ///
    /// Returns an error when `value` is zero or exceeds the hard resource bound.
    pub const fn new(value: usize) -> Result<Self, LaunchConcurrencyError> {
        match NonZeroUsize::new(value) {
            Some(value) if value.get() <= MAX_CONCURRENT_LAUNCHES => Ok(Self(value)),
            Some(value) => Err(LaunchConcurrencyError::TooLarge(value)),
            None => Err(LaunchConcurrencyError::Zero),
        }
    }

    /// Return the validated batch limit.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0.get()
    }
}

impl Default for LaunchConcurrency {
    fn default() -> Self {
        Self(NonZeroUsize::new(DEFAULT_MAX_CONCURRENT_LAUNCHES).unwrap_or(NonZeroUsize::MIN))
    }
}

/// Invalid concurrent supervisor-launch limit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LaunchConcurrencyError {
    /// A concurrency limit must permit at least one launch.
    Zero,
    /// The requested limit exceeds the hard resource bound.
    TooLarge(NonZeroUsize),
}

impl Display for LaunchConcurrencyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Zero => formatter.write_str("launch concurrency must be nonzero"),
            Self::TooLarge(value) => write!(
                formatter,
                "launch concurrency {} exceeds maximum {MAX_CONCURRENT_LAUNCHES}",
                value.get()
            ),
        }
    }
}

impl Error for LaunchConcurrencyError {}

/// Resource limits for one authoritative directory scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanLimits {
    /// Maximum number of top-level `*.yml` candidates.
    pub max_definitions: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_definitions: DEFAULT_MAX_DEFINITIONS,
        }
    }
}
