//! Checked process and process-group identity used throughout the broker.
//!
//! `ProcessId` and `ProcessGroupId` wrap the raw operating-system integers
//! `libc`/`fork` hand back after a successful fork or wait so a negative or
//! zero value — never a valid `waitpid`/`kill` selector — cannot silently flow
//! into a signal or wait call. The inner value stays `pub(super)` rather than
//! private: sibling modules in this facade (`daemon`, `spawn`, `broker`)
//! construct these types directly from an already-checked `fork` identifier,
//! and widening the field to the whole `process` module tree keeps that
//! construction infallible without re-validating a value `fork` already
//! guaranteed was positive.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

/// Checked positive process identifier valid only while Immortal owns the child.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessId(pub(super) i32);

impl ProcessId {
    /// Construct a process identifier only when the operating-system value is positive.
    #[must_use]
    pub const fn new(raw: i32) -> Option<Self> {
        if raw > 0 { Some(Self(raw)) } else { None }
    }

    /// Return the positive operating-system value for status and PID-file output.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl Display for ProcessId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

/// A raw process identifier was zero or negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProcessId(i32);

impl Display for InvalidProcessId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "process ID must be positive, got {}", self.0)
    }
}

impl Error for InvalidProcessId {}

impl TryFrom<i32> for ProcessId {
    type Error = InvalidProcessId;

    fn try_from(raw: i32) -> Result<Self, Self::Error> {
        Self::new(raw).ok_or(InvalidProcessId(raw))
    }
}

/// Checked positive process-group identifier owned by one service generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ProcessGroupId(pub(super) i32);

impl ProcessGroupId {
    pub(super) const fn new(raw: i32) -> Option<Self> {
        if raw > 0 { Some(Self(raw)) } else { None }
    }

    /// Return the positive operating-system value for diagnostics.
    #[must_use]
    pub const fn get(self) -> i32 {
        self.0
    }
}

impl Display for ProcessGroupId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        Display::fmt(&self.0, formatter)
    }
}

/// A raw process-group identifier was zero or negative.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidProcessGroupId(i32);

impl Display for InvalidProcessGroupId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "process-group ID must be positive, got {}",
            self.0
        )
    }
}

impl Error for InvalidProcessGroupId {}

impl TryFrom<i32> for ProcessGroupId {
    type Error = InvalidProcessGroupId;

    fn try_from(raw: i32) -> Result<Self, Self::Error> {
        Self::new(raw).ok_or(InvalidProcessGroupId(raw))
    }
}

/// Explicit signal target; raw negative PID conventions never cross this boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SignalTarget {
    /// Deliver to exactly one owned child.
    Process(ProcessId),
    /// Deliver to every current member of one owned generation group.
    Group(ProcessGroupId),
}

#[cfg(test)]
mod tests {
    use super::ProcessId;

    #[test]
    fn process_identifiers_reject_waitpid_selectors() {
        assert!(ProcessId::new(-1).is_none());
        assert!(ProcessId::new(0).is_none());
        assert_eq!(ProcessId::new(1).map(ProcessId::get), Some(1));
    }
}
