//! Broker-local identity, signal scope, and lifetime-cleanup contract types.
//!
//! [`BrokerTaskId`] distinguishes auxiliary hook and logger tasks from
//! ordinary service generations by reserving the high bit of the shared
//! [`Generation`] numbering space; [`BrokerSignalScope`] and
//! [`ReadinessFailure`] classify signal targets and readiness outcomes for
//! the supervisor. [`BrokerLifetimePlan`] is the fallback stop command and
//! pair of hard deadlines the broker invokes only if it loses its supervisor
//! connection while a descriptor-tracked generation remains active.

use std::io;
use std::time::Duration;

use crate::supervisor::Generation;

use super::ProcessCommand;

const AUXILIARY_GENERATION_BASE: u64 = 1_u64 << 63;
const MAX_LIFETIME_CLEANUP_TIMEOUT: Duration = Duration::from_hours(24);

/// Supervisor-local identifier for a broker child which is not a service generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrokerTaskId(u64);

impl BrokerTaskId {
    /// Construct an auxiliary identifier from the nonzero low-half counter.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 || value >= AUXILIARY_GENERATION_BASE {
            None
        } else {
            Some(Self(value))
        }
    }

    /// Return the supervisor-local task number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    pub(super) fn generation(self) -> Option<Generation> {
        Generation::new(AUXILIARY_GENERATION_BASE | self.0)
    }

    pub(super) fn from_generation(generation: Generation) -> Option<Self> {
        let raw = generation.get();
        if raw & AUXILIARY_GENERATION_BASE == 0 {
            None
        } else {
            Self::new(raw & !AUXILIARY_GENERATION_BASE)
        }
    }
}

/// Main child or complete generation-group target resolved inside the broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerSignalScope {
    Process,
    Group,
}

/// Stable reason a generation did not complete descriptor readiness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessFailure {
    Timeout,
    Descriptor,
    InvalidToken,
}

/// Broker-owned fallback needed to stop a descriptor generation after supervisor loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerLifetimePlan {
    pub(super) stop: ProcessCommand,
    pub(super) stop_timeout: Duration,
    pub(super) lifetime_timeout: Duration,
}

impl BrokerLifetimePlan {
    /// Build the materialized stop command and its two hard deadlines.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when either deadline is zero or exceeds 24 hours.
    pub fn new(
        stop: ProcessCommand,
        stop_timeout: Duration,
        lifetime_timeout: Duration,
    ) -> io::Result<Self> {
        if stop_timeout.is_zero()
            || lifetime_timeout.is_zero()
            || stop_timeout > MAX_LIFETIME_CLEANUP_TIMEOUT
            || lifetime_timeout > MAX_LIFETIME_CLEANUP_TIMEOUT
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "broker lifetime cleanup deadlines must be greater than zero and at most 24 hours",
            ));
        }
        Ok(Self {
            stop,
            stop_timeout,
            lifetime_timeout,
        })
    }
}
