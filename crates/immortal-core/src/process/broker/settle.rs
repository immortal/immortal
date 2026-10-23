//! Bounded wake-ups for the broker's shutdown and cleanup waits.
//!
//! Waiting for owned processes to exit and for killed groups to drain cannot
//! rely on `SIGCHLD` alone: a group member reaped by a parent outside the
//! broker sends the broker no signal, and without the child-subreaper role
//! (macOS) init reaps orphans silently. [`wait_for_change`] therefore wakes on
//! whichever comes first: `SIGCHLD`, a short poll that backs off from
//! [`POLL_START`] to [`POLL_MAX`], or the caller's deadline. Each caller drains
//! and re-checks its own condition after every wake, and once more at the
//! deadline, so a condition met in the final interval still counts.

use std::time::Duration;

use tokio::signal::unix::Signal as ChildSignal;
use tokio::time::{Instant, sleep_until};

use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};

/// First re-check delay after a wake without a child signal.
pub(super) const POLL_START: Duration = Duration::from_millis(5);
/// Longest re-check delay; keeps a silently drained group noticed promptly.
const POLL_MAX: Duration = Duration::from_millis(25);

/// Sleep until the next reason to re-check: `SIGCHLD`, the poll, or `deadline`.
///
/// `poll` is the delay before the next unsignalled re-check; it doubles after
/// each poll wake up to [`POLL_MAX`] and resets to [`POLL_START`] when a child
/// signal arrives. Both branches are cancellation-safe.
///
/// # Errors
///
/// Returns an error when the child-signal stream has closed, which leaves the
/// broker unable to observe exits.
pub(super) async fn wait_for_change(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    poll: &mut Duration,
) -> Result<(), ProcessBrokerError> {
    tokio::select! {
        signal = child_signal.recv() => {
            if signal.is_none() {
                return Err(ProcessBrokerError(ProcessBrokerErrorKind::SignalStreamClosed));
            }
            *poll = POLL_START;
        }
        () = sleep_until(wake_at(Instant::now(), *poll, deadline)) => {
            *poll = next_poll(*poll);
        }
    }
    Ok(())
}

/// The earlier of the next poll and the caller's deadline.
fn wake_at(now: Instant, poll: Duration, deadline: Instant) -> Instant {
    now.checked_add(poll)
        .map_or(deadline, |wake| wake.min(deadline))
}

/// Double a poll delay, capped at [`POLL_MAX`].
fn next_poll(poll: Duration) -> Duration {
    poll.saturating_mul(2).min(POLL_MAX)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::time::Duration;

    use tokio::signal::unix::{SignalKind, signal};
    use tokio::time::Instant;

    use super::{POLL_MAX, POLL_START, next_poll, wait_for_change, wake_at};

    #[test]
    fn polling_backs_off_to_its_cap() {
        let mut poll = POLL_START;
        for _ in 0..8 {
            poll = next_poll(poll);
        }
        assert_eq!(poll, POLL_MAX);
        assert_eq!(next_poll(Duration::MAX), POLL_MAX);
    }

    #[test]
    fn a_wake_never_passes_the_deadline() {
        let now = Instant::now();
        let deadline = now + Duration::from_millis(3);
        assert_eq!(wake_at(now, Duration::from_millis(10), deadline), deadline);
        assert_eq!(
            wake_at(now, Duration::from_millis(1), deadline),
            now + Duration::from_millis(1)
        );
    }

    /// Without any child signal the wait still returns after one poll, well
    /// before a distant deadline, so a silently drained group is noticed.
    #[tokio::test]
    async fn a_wait_without_child_signals_returns_after_one_poll() -> Result<(), Box<dyn Error>> {
        let mut child_signal = signal(SignalKind::child())?;
        let mut poll = POLL_START;
        let started = Instant::now();
        wait_for_change(
            started + Duration::from_secs(5),
            &mut child_signal,
            &mut poll,
        )
        .await?;
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(poll, next_poll(POLL_START));
        Ok(())
    }

    /// A wait whose deadline has already passed returns at once.
    #[tokio::test]
    async fn a_wait_past_its_deadline_returns_at_once() -> Result<(), Box<dyn Error>> {
        let mut child_signal = signal(SignalKind::child())?;
        let mut poll = POLL_MAX;
        let started = Instant::now();
        wait_for_change(started, &mut child_signal, &mut poll).await?;
        assert!(started.elapsed() < Duration::from_secs(1));
        Ok(())
    }
}
