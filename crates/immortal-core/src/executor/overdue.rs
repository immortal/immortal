//! Waiting for a late broker answer instead of giving up on supervision.
//!
//! Every state that waits for the broker to confirm something arms a deadline:
//! a service, condition, or hook start, a readiness outcome, and the reap that
//! ends a killed process. The broker bounds each of those operations itself, so
//! its answer is late while the broker is alive but not running: the machine is
//! starved, the host or virtual machine was paused, or the broker was stopped.
//! A reap can also be late because a kill was never delivered or a killed
//! process sits in uninterruptible sleep. A broker that died is noticed without
//! any deadline, as end of file or a broken pipe on its socket. A logger start
//! the broker confirms late is the exception: it is recorded as a failed
//! logger start under the logger's own restart policy.
//!
//! Exiting at such a deadline would help nothing. It would end the very
//! service the supervisor exists to keep, while waiting keeps the state that
//! protects it: no replacement starts beside a process whose end is not yet
//! confirmed, and no stop is reported before it happened. An expired wait
//! therefore reports itself on standard error, at most once a minute, and
//! waits again. Only broker death or a protocol violation ends supervision.
//!
//! A wait for the reap of a killed process also asks for the kill again every
//! few seconds, unless an earlier request is still unanswered. A helper
//! exchange that times out before its request is queued delivers nothing, so
//! the first kill may never have reached the group; the broker repeats only
//! such an undelivered kill, through the pin it still holds, and answers any
//! other repeat at once. The unanswered-request gate keeps a stalled broker's
//! socket from filling with repeats.
//!
//! An intended shutdown, a supervisor told to halt or exit or a TERM or INT
//! queued behind a lifecycle hook, keeps its bounded deadlines: the supervisor is ending anyway, and its broker teardown then
//! cleans up within bounds instead of leaving shutdown waiting for ever.
//!
//! A daemonized supervisor's standard error is `/dev/null`, so its reports are
//! visible only in the foreground or under a service manager that keeps the
//! stream.

use std::{collections::VecDeque, time::Duration};

use tokio::time::Instant as TokioInstant;

use super::{
    BrokerSignalScope, BrokerTaskId, DesiredState, Generation, OwedTaskAcknowledgements,
    PendingSignal, ProcessBrokerClient, ProcessBrokerError, ProcessSignal, StateMachine,
};

/// How often a wait that stays overdue is reported, and re-examined.
pub(super) const OVERDUE_REPORT_INTERVAL: Duration = Duration::from_mins(1);

/// How often a kill whose reap is overdue is asked for again.
pub(super) const KILL_RETRY_INTERVAL: Duration = Duration::from_secs(5);

/// Whether the supervisor is ending, so an overdue wait stays bounded: it was
/// told to halt or exit, or a TERM or INT is queued behind a lifecycle hook.
pub(super) const fn ending(machine: &StateMachine, shutdown_requested: bool) -> bool {
    shutdown_requested || matches!(machine.desired(), DesiredState::Halt | DesiredState::Exit)
}

/// Rate limit shared by every overdue report of one supervisor.
#[derive(Debug, Default)]
pub(super) struct OverdueReports {
    last: Option<TokioInstant>,
}

impl OverdueReports {
    /// Whether a report about `what` is due at `now`, recording it if so.
    ///
    /// At most one report is printed per `interval`, however many waits are
    /// overdue, so a stalled broker cannot flood standard error.
    pub(super) fn report(&mut self, what: &str, now: TokioInstant, interval: Duration) -> bool {
        if self
            .last
            .is_some_and(|last| now.saturating_duration_since(last) < interval)
        {
            return false;
        }
        self.last = Some(now);
        crate::diagnostic::report(format_args!(
            "immortal supervisor: the process broker has not confirmed {what} in time; still waiting"
        ));
        true
    }
}

/// Keep waiting for `what`: report it if a report is due, and examine the
/// wait again one report interval from now.
///
/// The new deadline always lies in the future, so an overdue wait can never
/// make the event loop spin.
pub(super) fn keep_waiting(
    reports: &mut OverdueReports,
    deadline: &mut Option<TokioInstant>,
    what: &str,
) {
    let now = TokioInstant::now();
    reports.report(what, now, OVERDUE_REPORT_INTERVAL);
    *deadline = Some(now + OVERDUE_REPORT_INTERVAL);
}

/// Keep waiting for the reap of a killed service group, asking for the kill
/// again unless an earlier service signal is still unanswered, and return the
/// deadline at which to examine the wait again.
///
/// # Errors
///
/// Returns a failure to write the request to the broker.
pub(super) async fn keep_killing_generation(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    pending_signals: &mut VecDeque<PendingSignal>,
    reports: &mut OverdueReports,
    what: &str,
) -> Result<TokioInstant, ProcessBrokerError> {
    if pending_signals.is_empty() {
        client
            .signal(generation, BrokerSignalScope::Group, ProcessSignal::Kill)
            .await?;
        pending_signals.push_back(PendingSignal::Lifecycle);
    }
    let now = TokioInstant::now();
    reports.report(what, now, OVERDUE_REPORT_INTERVAL);
    Ok(now + KILL_RETRY_INTERVAL)
}

/// Keep waiting for the reap of a killed task, asking for the kill again
/// unless the task still owes an answer, and return the deadline at which to
/// examine the wait again.
///
/// # Errors
///
/// Returns a failure to write the request to the broker.
pub(super) async fn keep_killing_task(
    client: &mut ProcessBrokerClient,
    task: BrokerTaskId,
    task_signals: &mut OwedTaskAcknowledgements,
    reports: &mut OverdueReports,
    what: &str,
) -> Result<TokioInstant, ProcessBrokerError> {
    if !task_signals.owes(task) {
        task_signals
            .signal(client, task, ProcessSignal::Kill)
            .await?;
    }
    let now = TokioInstant::now();
    reports.report(what, now, OVERDUE_REPORT_INTERVAL);
    Ok(now + KILL_RETRY_INTERVAL)
}

#[cfg(test)]
mod tests;
