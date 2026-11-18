//! Tests for how a launch batch waits for its launchers.
//!
//! A synthetic broker peer replays the task events a real broker sends, so the
//! batch's own bookkeeping is what is tested: it has no deadline, reports while
//! it waits, ignores a launcher's job control, and still refuses events for a
//! task it never submitted.

use std::{collections::BTreeMap, error::Error, time::Duration};

use tokio::{net::UnixStream, time::sleep, time::timeout};

use super::{LaunchTaskState, LauncherError, SupervisorLauncher};
use crate::process::{
    BrokerTaskId, ChildEvent, ProcessBrokerClient, ProcessBrokerEvent, ProcessGroupId, ProcessId,
};

/// Hard bound on every test, far above what any of them needs.
const TEST_DEADLINE: Duration = Duration::from_secs(10);

/// Report interval short enough for a test to observe several reports.
const REPORT_INTERVAL: Duration = Duration::from_millis(20);

fn pid() -> Result<ProcessId, Box<dyn Error>> {
    ProcessId::new(4321).ok_or_else(|| "invalid test pid".into())
}

fn launcher() -> Result<(SupervisorLauncher, UnixStream, BrokerTaskId), Box<dyn Error>> {
    let (ours, peer) = UnixStream::pair()?;
    let launcher = SupervisorLauncher {
        client: ProcessBrokerClient::for_test(ours, pid()?),
        next_task: 1,
    };
    let task = BrokerTaskId::new(1).ok_or("invalid test task")?;
    Ok((launcher, peer, task))
}

fn started(task: BrokerTaskId) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(ProcessBrokerEvent::TaskStarted {
        task,
        process: pid()?,
        group: ProcessGroupId::try_from(4321)?,
    })
}

fn child(task: BrokerTaskId, event: ChildEvent) -> ProcessBrokerEvent {
    ProcessBrokerEvent::TaskChild { task, event }
}

async fn send(peer: &mut UnixStream, event: &ProcessBrokerEvent) -> Result<(), Box<dyn Error>> {
    ProcessBrokerClient::write_test_event(peer, event).await?;
    Ok(())
}

/// Regression: a batch waited at most fifteen seconds and then ended the whole
/// directory manager, although every launch bounds itself and was only slow.
/// The batch now waits as long as its launches take and reports meanwhile.
#[tokio::test]
async fn a_launch_batch_waits_and_reports_while_a_launcher_is_slow() -> Result<(), Box<dyn Error>> {
    let (mut launcher, mut peer, task) = launcher()?;
    let mut states = BTreeMap::from([(task, LaunchTaskState::default())]);
    let mut reports = Vec::new();

    let finished = {
        let mut record = |remaining: usize, _: Duration| reports.push(remaining);
        timeout(TEST_DEADLINE, async {
            tokio::join!(
                launcher.wait_for_launch_batch(&mut states, REPORT_INTERVAL, &mut record),
                async {
                    send(&mut peer, &started(task)?).await?;
                    sleep(REPORT_INTERVAL * 6).await;
                    send(
                        &mut peer,
                        &child(
                            task,
                            ChildEvent::Exited {
                                pid: pid()?,
                                code: 0,
                            },
                        ),
                    )
                    .await
                },
            )
        })
        .await?
    };

    finished.0?;
    finished.1?;
    assert!(matches!(
        states.get(&task).and_then(|state| state.outcome.as_ref()),
        Some(Ok(()))
    ));
    assert!(reports.len() >= 2, "reports: {reports:?}");
    assert!(reports.iter().all(|&remaining| remaining == 1));
    Ok(())
}

/// Regression: a launcher's stop or continue reached the batch as an
/// unexpected event and ended the whole directory manager.
#[tokio::test]
async fn launcher_job_control_events_do_not_end_a_batch() -> Result<(), Box<dyn Error>> {
    let (mut launcher, mut peer, task) = launcher()?;
    let mut states = BTreeMap::from([(task, LaunchTaskState::default())]);
    send(&mut peer, &started(task)?).await?;
    send(
        &mut peer,
        &child(
            task,
            ChildEvent::Stopped {
                pid: pid()?,
                signal: 19,
            },
        ),
    )
    .await?;
    send(
        &mut peer,
        &child(task, ChildEvent::Continued { pid: pid()? }),
    )
    .await?;
    send(
        &mut peer,
        &child(
            task,
            ChildEvent::Exited {
                pid: pid()?,
                code: 0,
            },
        ),
    )
    .await?;

    timeout(
        TEST_DEADLINE,
        launcher.wait_for_launch_batch(&mut states, REPORT_INTERVAL, &mut |_, _| {}),
    )
    .await??;

    assert!(matches!(
        states.get(&task).and_then(|state| state.outcome.as_ref()),
        Some(Ok(()))
    ));
    Ok(())
}

/// An event for a task the batch never submitted is still a protocol fault.
#[tokio::test]
async fn an_event_for_an_unknown_launch_task_stays_fatal() -> Result<(), Box<dyn Error>> {
    let (mut launcher, mut peer, task) = launcher()?;
    let stranger = BrokerTaskId::new(99).ok_or("invalid test task")?;
    let mut states = BTreeMap::from([(task, LaunchTaskState::default())]);
    send(
        &mut peer,
        &child(
            stranger,
            ChildEvent::Stopped {
                pid: pid()?,
                signal: 19,
            },
        ),
    )
    .await?;

    let outcome = timeout(
        TEST_DEADLINE,
        launcher.wait_for_launch_batch(&mut states, REPORT_INTERVAL, &mut |_, _| {}),
    )
    .await?;

    assert!(matches!(outcome, Err(LauncherError::UnexpectedEvent(_))));
    Ok(())
}
