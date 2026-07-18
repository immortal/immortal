//! Best-effort external sampling of a live process's resource footprint.
//!
//! The resource-soak contract observes the broker's own memory, descriptor, and
//! child trends while it is driven under sustained fault load. Sampling shells
//! out to base-system utilities (`ps` plus a per-platform descriptor lister)
//! rather than reading another process's kernel state directly, so it stays
//! portable across Linux, macOS, and FreeBSD without unsafe code or an extra
//! platform crate. Every probe is best effort: an unavailable source yields
//! `None` so a platform that cannot supply one metric degrades to fewer signals
//! instead of producing a false regression.

use std::process::Command;

use immortal_core::process::ProcessId;

/// One point-in-time observation of an owned child process.
///
/// Each field is `None` when its source was unavailable on this platform or the
/// process had already exited, so callers assert only on the metrics they could
/// actually sample.
#[derive(Clone, Copy, Debug, Default)]
pub struct ResourceSample {
    /// Resident set size in kibibytes, as reported by `ps -o rss=`.
    pub resident_kib: Option<u64>,
    /// Direct child processes of the target, including unreaped zombies.
    pub child_processes: Option<u64>,
    /// Open descriptors held by the target, from the per-platform source.
    pub open_descriptors: Option<u64>,
}

/// Sample every available metric for one owned process in a single call.
#[must_use]
pub fn sample(process: ProcessId) -> ResourceSample {
    ResourceSample {
        resident_kib: resident_kib(process),
        child_processes: child_processes(process),
        open_descriptors: open_descriptors(process),
    }
}

/// Return the resident set size in kibibytes, or `None` when `ps` cannot report it.
#[must_use]
pub fn resident_kib(process: ProcessId) -> Option<u64> {
    let mut command = Command::new("ps");
    command
        .args(["-o", "rss=", "-p"])
        .arg(process.get().to_string());
    let stdout = command_stdout(&mut command)?;
    stdout.split_whitespace().next()?.parse::<u64>().ok()
}

/// Count the target's direct children, or `None` when `ps` is unavailable.
///
/// Unreaped zombies still list the target as their parent, so a growing count
/// exposes a reaping or ownership leak rather than healthy churn.
#[must_use]
pub fn child_processes(process: ProcessId) -> Option<u64> {
    let mut command = Command::new("ps");
    command.args(["-A", "-o", "ppid="]);
    let stdout = command_stdout(&mut command)?;
    let parent = i64::from(process.get());
    let count = stdout
        .lines()
        .filter_map(|line| line.trim().parse::<i64>().ok())
        .filter(|ppid| *ppid == parent)
        .count();
    u64::try_from(count).ok()
}

/// Count the target's open descriptors from `/proc`, or `None` when unreadable.
#[cfg(target_os = "linux")]
#[must_use]
pub fn open_descriptors(process: ProcessId) -> Option<u64> {
    let path = format!("/proc/{}/fd", process.get());
    let count = std::fs::read_dir(path).ok()?.flatten().count();
    u64::try_from(count).ok()
}

/// Count the target's open descriptors from `procstat -f`, or `None` on failure.
#[cfg(target_os = "freebsd")]
#[must_use]
pub fn open_descriptors(process: ProcessId) -> Option<u64> {
    let mut command = Command::new("procstat");
    command.args(["-f"]).arg(process.get().to_string());
    let stdout = command_stdout(&mut command)?;
    // The first line is a column header; every later non-empty line is one
    // descriptor, so the count tracks the descriptor trend without parsing
    // procstat's exact columns.
    let count = stdout
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count();
    u64::try_from(count).ok()
}

/// Count the target's open descriptors from `lsof`, or `None` on failure.
#[cfg(target_os = "macos")]
#[must_use]
pub fn open_descriptors(process: ProcessId) -> Option<u64> {
    let mut command = Command::new("lsof");
    command.args(["-p"]).arg(process.get().to_string());
    let stdout = command_stdout(&mut command)?;
    // The first line is a column header; every later non-empty line is one open
    // file, so the count tracks the descriptor trend.
    let count = stdout
        .lines()
        .skip(1)
        .filter(|line| !line.trim().is_empty())
        .count();
    u64::try_from(count).ok()
}

/// Descriptor sampling is unavailable on platforms without a supported source.
#[cfg(not(any(target_os = "linux", target_os = "freebsd", target_os = "macos")))]
#[must_use]
pub fn open_descriptors(_process: ProcessId) -> Option<u64> {
    None
}

/// Run one probe and return its standard output only when it exits successfully.
fn command_stdout(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    if output.status.success() {
        String::from_utf8(output.stdout).ok()
    } else {
        None
    }
}
