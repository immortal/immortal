//! Bounded subprocesses and private scratch space for preload contracts.
//!
//! A command remains this fixture's exclusive direct child through collection.
//! Timeout cleanup checks its still-owned Child before killing and collecting it;
//! callers own descendant release protocols and declare scratch before children.

use std::{
    env, fs, io,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    process::{Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

/// Private artifacts removed after all commands and their fixtures have finished.
pub struct Scratch(pub PathBuf);

impl Scratch {
    /// Create an owner-only directory below the caller's configured TMPDIR.
    pub fn new(name: &str) -> io::Result<Self> {
        let path = env::temp_dir().join(format!("immortal-{name}-{}", std::process::id()));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Run one exclusive direct child under a deadline, collecting it on every path.
/// Returns spawn/wait errors or `TimedOut` after killing an overdue child.
pub fn run(command: &mut Command, bound: Duration) -> io::Result<ExitStatus> {
    let mut child = command.spawn()?;
    let deadline = Instant::now() + bound;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            outcome => {
                if matches!(child.try_wait(), Ok(None)) {
                    let _ = child.kill();
                    let _ = child.wait();
                }
                return Err(outcome.err().unwrap_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::TimedOut,
                        "contract command exceeded deadline",
                    )
                }));
            }
        }
    }
}
