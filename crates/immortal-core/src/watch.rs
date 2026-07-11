//! Native filesystem notifications as debounced full-reconciliation triggers.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    path::Path,
    time::Duration,
};

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::{
    sync::mpsc,
    time::{Instant, Interval, MissedTickBehavior, interval_at, sleep_until},
};

/// Bounded native events retained until the next full reconciliation.
pub const WATCH_EVENT_CAPACITY: usize = 256;
/// Default editor-event debounce interval.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(250);
/// Periodic recovery scan for dropped/coalesced native events.
pub const DEFAULT_SAFETY_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// Why a complete authoritative scan is required.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TriggerReason {
    /// Startup scan before relying on notifications.
    Initial,
    /// One or more native events survived debounce.
    Filesystem,
    /// Periodic recovery scan independent of notification delivery.
    Periodic,
}

/// One scan trigger plus non-fatal watcher diagnostics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReconcileTrigger {
    /// Trigger category.
    pub reason: TriggerReason,
    /// Watcher errors observed during this debounce window.
    pub watcher_errors: Vec<String>,
}

/// Failure to initialize native directory monitoring.
#[derive(Debug)]
pub struct WatchError(notify::Error);

impl Display for WatchError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unable to watch definitions directory: {}",
            self.0
        )
    }
}

impl Error for WatchError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Owns the native watcher and emits only authoritative-scan triggers.
pub struct ReconcileTriggers {
    _watcher: RecommendedWatcher,
    events: mpsc::Receiver<notify::Result<Event>>,
    periodic: Interval,
    debounce: Duration,
    initial: bool,
}

impl ReconcileTriggers {
    /// Watch one directory non-recursively with production defaults.
    ///
    /// # Errors
    ///
    /// Returns an error if the native watcher cannot be created or registered.
    pub fn new(directory: &Path) -> Result<Self, WatchError> {
        Self::with_intervals(directory, DEFAULT_DEBOUNCE, DEFAULT_SAFETY_SCAN_INTERVAL)
    }

    /// Watch with explicit debounce and safety-scan intervals.
    ///
    /// # Errors
    ///
    /// Returns an error for zero intervals or when the native watcher cannot be
    /// created or registered.
    pub fn with_intervals(
        directory: &Path,
        debounce: Duration,
        safety_scan_interval: Duration,
    ) -> Result<Self, WatchError> {
        if debounce.is_zero() || safety_scan_interval.is_zero() {
            return Err(WatchError(notify::Error::generic(
                "watch intervals must be greater than zero",
            )));
        }
        let (sender, events) = mpsc::channel(WATCH_EVENT_CAPACITY);
        let mut watcher = notify::recommended_watcher(move |event| {
            // A full periodic scan is authoritative, so a saturated channel may
            // safely coalesce additional hints instead of blocking this thread.
            let _ignored = sender.try_send(event);
        })
        .map_err(WatchError)?;
        watcher
            .watch(directory, RecursiveMode::NonRecursive)
            .map_err(WatchError)?;
        let mut periodic = interval_at(Instant::now() + safety_scan_interval, safety_scan_interval);
        periodic.set_missed_tick_behavior(MissedTickBehavior::Delay);
        Ok(Self {
            _watcher: watcher,
            events,
            periodic,
            debounce,
            initial: true,
        })
    }

    /// Wait for the next reason to perform a complete directory scan.
    pub async fn next(&mut self) -> ReconcileTrigger {
        if self.initial {
            self.initial = false;
            return ReconcileTrigger {
                reason: TriggerReason::Initial,
                watcher_errors: Vec::new(),
            };
        }

        tokio::select! {
            event = self.events.recv() => {
                let mut errors = Vec::new();
                record_error(event, &mut errors);
                self.debounce_events(errors).await
            }
            _ = self.periodic.tick() => ReconcileTrigger {
                reason: TriggerReason::Periodic,
                watcher_errors: Vec::new(),
            }
        }
    }

    async fn debounce_events(&mut self, mut errors: Vec<String>) -> ReconcileTrigger {
        let mut deadline = Instant::now() + self.debounce;
        loop {
            tokio::select! {
                () = sleep_until(deadline) => {
                    return ReconcileTrigger {
                        reason: TriggerReason::Filesystem,
                        watcher_errors: errors,
                    };
                }
                event = self.events.recv() => {
                    record_error(event, &mut errors);
                    deadline = Instant::now() + self.debounce;
                }
                _ = self.periodic.tick() => {
                    return ReconcileTrigger {
                        reason: TriggerReason::Periodic,
                        watcher_errors: errors,
                    };
                }
            }
        }
    }
}

fn record_error(event: Option<notify::Result<Event>>, errors: &mut Vec<String>) {
    if let Some(Err(error)) = event {
        errors.push(error.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use tokio::time::timeout;

    use super::{ReconcileTriggers, TriggerReason};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("immortal-watch-{}-{sequence}", std::process::id()));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn emits_initial_then_native_full_scan_trigger() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let mut triggers = ReconcileTriggers::with_intervals(
            directory.path(),
            Duration::from_millis(10),
            Duration::from_secs(30),
        )?;
        assert_eq!(triggers.next().await.reason, TriggerReason::Initial);
        fs::write(
            directory.path().join("api.yml"),
            b"version: 2\ncommand: [/bin/true]\n",
        )?;

        let trigger = timeout(Duration::from_secs(2), triggers.next()).await?;
        assert_eq!(trigger.reason, TriggerReason::Filesystem);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn periodic_scan_does_not_depend_on_notifications() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let mut triggers = ReconcileTriggers::with_intervals(
            directory.path(),
            Duration::from_millis(2),
            Duration::from_millis(10),
        )?;
        assert_eq!(triggers.next().await.reason, TriggerReason::Initial);
        let trigger = timeout(Duration::from_secs(1), triggers.next()).await?;
        assert_eq!(trigger.reason, TriggerReason::Periodic);
        Ok(())
    }

    #[test]
    fn rejects_zero_intervals() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        assert!(
            ReconcileTriggers::with_intervals(
                directory.path(),
                Duration::ZERO,
                Duration::from_secs(30),
            )
            .is_err()
        );
        Ok(())
    }
}
