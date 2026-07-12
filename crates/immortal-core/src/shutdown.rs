//! Owned Unix termination-signal intake for long-running Immortal components.
//!
//! One event-loop owner installs the TERM and INT streams before it begins
//! waiting for work. [`TerminationSignals::recv`] mutably borrows that owner
//! for one cancellation-safe wait; callers decide the safe lifecycle boundary
//! at which resource shutdown occurs.

use std::io;

use tokio::signal::unix::{Signal, SignalKind, signal};

/// Exclusive TERM and INT signal streams for one event loop.
pub struct TerminationSignals {
    interrupt: Signal,
    terminate: Signal,
}

impl TerminationSignals {
    /// Install TERM and INT listeners for the current process.
    ///
    /// # Errors
    ///
    /// Returns the operating-system error when either signal stream cannot be
    /// installed.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            interrupt: signal(SignalKind::interrupt())?,
            terminate: signal(SignalKind::terminate())?,
        })
    }

    /// Wait until TERM or INT is received.
    ///
    /// # Errors
    ///
    /// Returns `BrokenPipe` if the selected Tokio signal stream closes without
    /// delivering a signal.
    pub async fn recv(&mut self) -> io::Result<()> {
        let received = tokio::select! {
            received = self.interrupt.recv() => received,
            received = self.terminate.recv() => received,
        };
        received.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "termination signal stream closed",
            )
        })
    }
}
