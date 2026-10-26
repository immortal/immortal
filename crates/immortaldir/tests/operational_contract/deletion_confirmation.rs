//! Deletion-confirmation polling beside a manager that rewrites the tracker.
//!
//! The cross-restart phase reads the persisted tracker from outside the running
//! manager, which atomically replaces it on every pass. The snapshot reader
//! fails closed with `InvalidData` when the file it opened is replaced before or
//! during the read. That protects the manager's own startup read, but it is an
//! expected transient for this concurrent observer, so the poll retries it and
//! reports the last such error only at its deadline; persistently malformed
//! state still fails the contract. The regression phase replaces the tracker
//! continuously while the poll runs and requires the confirmation to be seen.

use std::{
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    config::parse_file,
    reconcile::{
        DefinitionSnapshots, DesiredStateTracker, ReconcileAction, ScanLimits, ScanResult,
        scan_directory,
    },
};
use tokio::{runtime::Builder, time::sleep};

use super::DEADLINE;

/// Interval between tracker reads in the operational contract.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// The regression polls back to back with a zero interval, so a read is almost
/// always in progress when the writer thread replaces the tracker. A nonzero
/// interval would depend on platform timer granularity and save speed.
const RACING_POLL_INTERVAL: Duration = Duration::ZERO;

/// How long the regression writer checkpoints the service as still present,
/// so the poll must read through many replacements before it can succeed.
const PRESENT_PHASE: Duration = Duration::from_secs(5);

/// Wait until the persisted tracker confirms `name`'s deletion on the next scan.
///
/// A tracker replaced while it is read fails with `InvalidData`; the poll
/// retries it and reports it only when the deadline passes without a readable
/// confirmation. Any other error fails at once.
pub(super) async fn wait_for_first_deletion_confirmation(
    runtime: &Path,
    name: &str,
) -> Result<(), Box<dyn Error>> {
    poll_deletion_confirmation(runtime, name, POLL_INTERVAL).await
}

async fn poll_deletion_confirmation(
    runtime: &Path,
    name: &str,
    interval: Duration,
) -> Result<(), Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        let snapshots = DefinitionSnapshots::open(runtime)?;
        let unreadable = match snapshots.load_tracker() {
            Ok(mut persisted) => {
                if persisted.apply(&ScanResult::default()).get(name) == Some(&ReconcileAction::Stop)
                {
                    return Ok(());
                }
                None
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => Some(error),
            Err(error) => return Err(error.into()),
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(match unreadable {
                Some(error) => format!("first deletion confirmation was unreadable: {error}"),
                None => "first deletion confirmation was not persisted".to_owned(),
            }
            .into());
        }
        sleep(interval).await;
    }
}

/// Prove the poll observes a confirmation while the tracker is replaced nonstop.
///
/// The writer first checkpoints the service as present and then one absence,
/// which the next scan confirms. The poll shares the contract's retry and only
/// reads more often. Without the retry, the first read that races a replacement
/// ends the poll with `InvalidData`. The writer stops when the poll returns and
/// is bounded by the contract deadline.
pub(super) fn prove_confirmation_poll_tolerates_replacement(
    root: &Path,
) -> Result<(), Box<dyn Error>> {
    let definitions = root.join("replacement-definitions");
    let runtime = root.join("replacement-runtime");
    fs::create_dir(&definitions)?;
    fs::set_permissions(&definitions, fs::Permissions::from_mode(0o755))?;
    fs::create_dir(&runtime)?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    let definition = definitions.join("replaced.yml");
    fs::write(&definition, "version: 2\ncommand: [/bin/true]\n")?;
    let snapshots = DefinitionSnapshots::open(&runtime)?;
    snapshots.record_applied("replaced", &parse_file(&definition)?)?;
    let mut present = DesiredStateTracker::default();
    present.apply(&scan_directory(&definitions, ScanLimits::default())?);
    let mut absent = present.clone();
    absent.apply(&ScanResult::default());

    let tokio = Builder::new_current_thread().enable_all().build()?;
    let stop = AtomicBool::new(false);
    thread::scope(|scope| -> Result<(), Box<dyn Error>> {
        let writer = scope.spawn(|| replace_tracker(&snapshots, &present, &absent, &stop));
        let polled = tokio.block_on(poll_deletion_confirmation(
            &runtime,
            "replaced",
            RACING_POLL_INTERVAL,
        ));
        stop.store(true, Ordering::Relaxed);
        let written = writer.join().map_err(|_| "tracker writer panicked")?;
        polled?;
        written?;
        Ok(())
    })
}

/// Replace the tracker in a tight loop: present first, then one absence.
fn replace_tracker(
    snapshots: &DefinitionSnapshots,
    present: &DesiredStateTracker,
    absent: &DesiredStateTracker,
    stop: &AtomicBool,
) -> io::Result<()> {
    let started = Instant::now();
    while !stop.load(Ordering::Relaxed) && started.elapsed() < DEADLINE {
        let tracker = if started.elapsed() < PRESENT_PHASE {
            present
        } else {
            absent
        };
        snapshots.record_tracker(tracker)?;
    }
    Ok(())
}
