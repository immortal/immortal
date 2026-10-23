//! Cleanup without granting signal authority to a group-existence probe.
//!
//! Broker-owned fixtures use private release files, never numeric signals. A
//! direct-child fixture may instead retain its leader PID: an exact wait checks
//! ownership before delivery and terminal collection retires that authority.
//! This guard collects only that exact leader, never another fixture's broker.

use std::{
    fs, io,
    path::PathBuf,
    thread,
    time::{Duration, Instant},
};

use immortal_core::process::{
    ProcessGroupId, ProcessId, ProcessSignal, SignalTarget, children_are_waitable,
    process_group_exists, reap_event, signal,
};

/// One fixture's private release protocol or directly owned leader.
pub struct GroupGuard {
    armed: bool,
    group: ProcessGroupId,
    release: Option<Vec<PathBuf>>,
    poll_interval: Duration,
    timeout: Duration,
}

impl GroupGuard {
    /// Retain an exact direct-child leader; construction alone grants no authority.
    pub const fn new(group: ProcessGroupId, timeout: Duration, poll_interval: Duration) -> Self {
        Self {
            armed: true,
            group,
            release: None,
            poll_interval,
            timeout,
        }
    }

    /// Release an indirect fixture through files it promises to observe until exit.
    /// The directory must outlive this guard. Group probes are observation only.
    pub fn released(
        group: ProcessGroupId,
        paths: Vec<PathBuf>,
        timeout: Duration,
        poll_interval: Duration,
    ) -> Self {
        let mut guard = Self::new(group, timeout, poll_interval);
        guard.release = Some(paths);
        guard
    }

    /// Retire cleanup after the scenario has proved completion.
    pub const fn disarm(&mut self) {
        self.armed = false;
    }

    /// Release the fixture and await its disappearance, without unpinned signals.
    ///
    /// # Errors
    /// Returns release, policy, exact-child wait, delivery or bounded wait errors.
    pub fn kill_and_wait(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        if let Some(paths) = &self.release {
            for path in paths {
                fs::write(path, "release")?;
            }
        } else {
            if !children_are_waitable()? {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "fixture requires waitable children",
                ));
            }
            let leader = ProcessId::try_from(self.group.get()).map_err(io::Error::other)?;
            match reap_event(leader) {
                Ok(Some(event)) if event.is_terminal() => {
                    self.disarm();
                    return Ok(());
                }
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                    self.disarm();
                    return Ok(());
                }
                Err(error) => return Err(error),
                Ok(_) => {}
            }
            // The unreaped leader PID equals the group ID: even an exit or
            // membership change cannot release this numeric ID before collection.
            signal(SignalTarget::Group(self.group), ProcessSignal::Kill)?;
        }
        let deadline = Instant::now() + self.timeout;
        loop {
            if self.release.is_none() {
                let leader = ProcessId::try_from(self.group.get()).map_err(io::Error::other)?;
                match reap_event(leader) {
                    Ok(Some(event)) if event.is_terminal() => {}
                    Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {}
                    Err(error) => return Err(error),
                    Ok(_) => {}
                }
            }
            if !process_group_exists(self.group)? {
                self.disarm();
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "released fixture group did not drain",
                ));
            }
            thread::sleep(self.poll_interval);
        }
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        let _ = self.kill_and_wait();
    }
}
