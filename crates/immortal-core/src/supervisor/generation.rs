//! Monotonic child-generation identity.
//!
//! A [`Generation`] is a nonzero counter naming one child lifecycle attempt.
//! It is created before supervision begins, advanced on each restart, and
//! carried through status and the control protocol so that stale lifecycle
//! events can never mutate a newer child.

use super::TransitionError;

/// Monotonic identity assigned to each child generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Generation(pub(super) u64);

impl Generation {
    /// First valid child generation.
    pub const FIRST: Self = Self(1);

    /// Construct a generation from a nonzero persisted or test value.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 { None } else { Some(Self(value)) }
    }

    /// Return the numeric value used for status and protocol messages.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Reconstruct a nonzero generation received from the validated control protocol.
    #[must_use]
    pub(crate) const fn from_protocol(value: u64) -> Self {
        Self(value)
    }

    pub(super) fn next(self) -> Result<Self, TransitionError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(TransitionError::GenerationExhausted)
    }
}
