//! Ordered service and logger shutdown state machine.
//!
//! Shutdown owns the externally visible phase and accepts only the safe
//! service-before-drain-before-loggers transition sequence. Invalid events are
//! reported as typed logging errors rather than weakening pipe-drain ordering.

use super::LoggingError;

/// Ordered shutdown phase for service and logging processes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LoggingShutdownPhase {
    /// Normal operation.
    #[default]
    Running,
    /// Service group must stop before pipe writers are closed.
    StoppingService,
    /// Service is reaped; logger chain drains remaining pipe bytes.
    Draining,
    /// Drain completed or timed out; logger stages may stop downstream-first.
    StoppingLoggers,
    /// Every logging child has been reaped.
    Complete,
}

/// Work selected by one shutdown transition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoggingShutdownEffect {
    /// Stop the service process group.
    StopService,
    /// Close service-side pipe writers and wait for logger drain.
    BeginDrain,
    /// Stop logger processes from final consumer back toward the service.
    StopLoggersDownstreamFirst,
    /// Shutdown is complete.
    Complete,
}

/// Enforces service-before-drain-before-logger shutdown ordering.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct LoggingShutdown {
    phase: LoggingShutdownPhase,
}

impl LoggingShutdown {
    /// Current externally reportable phase.
    #[must_use]
    pub const fn phase(self) -> LoggingShutdownPhase {
        self.phase
    }

    /// Start shutdown, accounting for a service which may already be absent.
    ///
    /// # Errors
    ///
    /// Returns an error unless shutdown is starting from normal operation.
    pub fn begin(&mut self, service_is_live: bool) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::Running {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        if service_is_live {
            self.phase = LoggingShutdownPhase::StoppingService;
            Ok(LoggingShutdownEffect::StopService)
        } else {
            self.phase = LoggingShutdownPhase::Draining;
            Ok(LoggingShutdownEffect::BeginDrain)
        }
    }

    /// Record complete service-group reaping and begin drain.
    ///
    /// # Errors
    ///
    /// Returns an error unless the service was being stopped.
    pub fn service_stopped(&mut self) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::StoppingService {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        self.phase = LoggingShutdownPhase::Draining;
        Ok(LoggingShutdownEffect::BeginDrain)
    }

    /// Record successful drain or a bounded drain timeout.
    ///
    /// # Errors
    ///
    /// Returns an error unless bytes are currently draining.
    pub fn drain_finished(&mut self) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::Draining {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        self.phase = LoggingShutdownPhase::StoppingLoggers;
        Ok(LoggingShutdownEffect::StopLoggersDownstreamFirst)
    }

    /// Record complete logger reaping.
    ///
    /// # Errors
    ///
    /// Returns an error unless logger shutdown is in progress.
    pub fn loggers_stopped(&mut self) -> Result<LoggingShutdownEffect, LoggingError> {
        if self.phase != LoggingShutdownPhase::StoppingLoggers {
            return Err(LoggingError::InvalidShutdownTransition);
        }
        self.phase = LoggingShutdownPhase::Complete;
        Ok(LoggingShutdownEffect::Complete)
    }
}
