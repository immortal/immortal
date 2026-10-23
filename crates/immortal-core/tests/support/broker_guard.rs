//! One owner for every broker-child observation, signal and terminal event.
//!
//! Exact waits cannot steal another fixture's child. Auxiliary fixture actions
//! borrow this guard rather than independently consuming broker events. A terminal
//! event is cached for later wait/Drop, and permanently retires signal authority.
//! A final scenario-level subtree drain collects orphans after all owners drop.

use std::{
    io, thread,
    time::{Duration, Instant},
};

use immortal_core::process::{ChildEvent, ProcessId, ProcessSignal, reap_event, signal_child};

/// Collect one broker child; cached terminal state survives every auxiliary action.
pub struct BrokerGuard {
    poll_interval: Duration,
    process: ProcessId,
    terminal: Option<ChildEvent>,
    timeout: Duration,
}

impl BrokerGuard {
    /// Record the direct child this fixture exclusively owns.
    pub const fn new(process: ProcessId, timeout: Duration, poll_interval: Duration) -> Self {
        Self {
            poll_interval,
            process,
            terminal: None,
            timeout,
        }
    }

    /// Observe an exact event, caching terminal collection for every later caller.
    /// Cached terminal events never invoke another wait.
    ///
    /// # Errors
    /// Returns exact-child wait errors when no terminal event was cached.
    pub fn observe(&mut self) -> io::Result<Option<ChildEvent>> {
        if self.terminal.is_some() {
            return Ok(self.terminal);
        }
        let event = reap_event(self.process)?;
        if event.is_some_and(ChildEvent::is_terminal) {
            self.terminal = event;
        }
        Ok(event)
    }

    /// Signal only while owned, recording any terminal event consumed instead.
    /// A cached terminal retires the PID: later requests return that event
    /// without any signal syscall.
    ///
    /// # Errors
    /// Returns policy, ownership or delivery errors.
    pub fn signal(&mut self, requested: ProcessSignal) -> io::Result<Option<ChildEvent>> {
        if self.terminal.is_some() {
            return Ok(self.terminal);
        }
        let terminal = signal_child(self.process, requested)?;
        self.terminal = terminal;
        Ok(terminal)
    }

    /// Collect a successful broker exit within the fixture deadline.
    /// May use a terminal event cached by an auxiliary fixture action.
    ///
    /// # Errors
    /// Returns wait, unsuccessful-exit or timeout errors.
    pub fn wait(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + self.timeout;
        loop {
            if let Some(event) = self.observe()?
                && event.is_terminal()
            {
                return match event {
                    ChildEvent::Exited { code: 0, .. } => Ok(()),
                    _ => Err(io::Error::other(format!(
                        "broker terminated unsuccessfully: {event:?}"
                    ))),
                };
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for process broker",
                ));
            }
            thread::sleep(self.poll_interval);
        }
    }
}

/// Kill only a still-owned broker and collect only its own terminal event.
impl Drop for BrokerGuard {
    fn drop(&mut self) {
        if self.terminal.is_some() {
            return;
        }
        match self.signal(ProcessSignal::Kill) {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {}
        }
        let _ = self.wait();
    }
}
