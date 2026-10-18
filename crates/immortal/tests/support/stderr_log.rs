//! One append-only log of supervisor stderr for a contract process.
//!
//! Contracts used to discard supervisor stderr, so an unexpected exit status
//! carried no diagnosis: a rare FreeBSD failure exited 70 with its
//! `immortal: <error>` line lost. Each spawn resets the file and the new
//! supervisor appends to it, as does its inherited broker, so a failed status
//! or deadline assertion quotes only the supervisor which just ran rather than
//! earlier scenarios' expected errors. The guard removes the file when the
//! contract returns, on success or failure.

use std::{
    fs, io,
    path::{Path, PathBuf},
    process::{self, Stdio},
};

/// Newest log lines quoted in a failure message.
const TAIL_LINES: usize = 20;

pub struct StderrLog;

impl StderrLog {
    /// Create an empty log for this process, replacing any stale file.
    pub fn create() -> io::Result<Self> {
        fs::File::create(path())?;
        Ok(Self)
    }

    /// Reset the log and open it as the appending stderr of one spawned supervisor.
    pub fn stdio() -> io::Result<Stdio> {
        fs::File::create(path())?;
        fs::OpenOptions::new()
            .append(true)
            .open(path())
            .map(Stdio::from)
    }

    /// The newest captured lines, empty when nothing was written.
    pub fn tail() -> String {
        let log = fs::read_to_string(path()).unwrap_or_default();
        let lines: Vec<&str> = log.lines().collect();
        let start = lines.len().saturating_sub(TAIL_LINES);
        lines
            .get(start..)
            .map_or_else(String::new, |tail| tail.join("\n"))
    }
}

impl Drop for StderrLog {
    fn drop(&mut self) {
        let _ = fs::remove_file(path());
    }
}

fn path() -> PathBuf {
    Path::new("/tmp").join(format!("im-fg-stderr-{}.log", process::id()))
}
