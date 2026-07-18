//! Black-box contract for the process broker's child-subreaper hygiene.
//!
//! Once the broker acquires the child-subreaper role, orphaned descendants of a
//! supervised service reparent to it instead of leaking to init. Each scenario
//! here forks a real broker, drives it through a genuine orphaning sequence, and
//! proves the same three-part contract: an adopted orphan is reaped as hygiene,
//! it never surfaces as a workload event, and it never aborts the broker. Every
//! scenario ends by spawning or signaling another service so a surviving,
//! responsive broker is a positive assertion rather than an absence of errors —
//! the reap classification regressed before this suite existed by treating any
//! unowned reap as a fatal ownership violation.
//!
//! One single-threaded parent owns the broker, its client, and every deadline.
//! [`BrokerGuard`] terminates and reaps the broker on every success and failure
//! path, and each `next_event` wait is bounded so a stuck broker fails fast
//! instead of hanging the suite.

#[path = "support/broker_guard.rs"]
mod broker_guard;

use std::{error::Error, io, time::Duration};

use immortal_core::{
    process::{
        BrokerSignalScope, ChildEvent, ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand,
        ProcessSignal, start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

fn main() -> Result<(), Box<dyn Error>> {
    run_orphan_scenario(group_killed_orphan_is_hygiene)?;
    run_orphan_scenario(self_exiting_orphan_is_hygiene)?;
    run_orphan_scenario(orphan_does_not_disturb_owned_service)?;
    Ok(())
}

/// Fork a fresh broker, run one orphaning scenario, and prove a clean shutdown.
///
/// The broker start, readiness handshake, scenario body, and shutdown all share
/// this single-threaded runtime and the same [`BrokerGuard`], so a scenario that
/// leaves the broker wedged still terminates and reaps it. The scenario owns
/// only a borrow of the client; the shutdown handshake stays here so every
/// scenario proves the broker returns to a quiescent, closeable state.
fn run_orphan_scenario<S>(scenario: S) -> Result<(), Box<dyn Error>>
where
    S: AsyncFnOnce(&mut ProcessBrokerClient) -> Result<(), Box<dyn Error>>,
{
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect_event(&mut client, "broker readiness", |event| {
            matches!(event, ProcessBrokerEvent::Ready)
        })
        .await?;
        scenario(&mut client).await?;
        client.shutdown().await?;
        expect_event(&mut client, "broker shutdown completion", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownComplete)
        })
        .await?;
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(runtime);
    broker.wait()?;
    Ok(())
}

/// A foreground service backgrounds a long-lived grandchild, then exits.
///
/// Tearing the generation's process group down kills the grandchild, which has
/// already reparented to the broker and is reaped as an adopted orphan. The
/// follow-up service proves the reap emitted no stray event and left the broker
/// responsive: its start must be the very next event the client observes.
async fn group_killed_orphan_is_hygiene(
    client: &mut ProcessBrokerClient,
) -> Result<(), Box<dyn Error>> {
    let service = generation(1)?;
    let mut command = ProcessCommand::new("/bin/sh");
    command.argument("-c").argument("/bin/sleep 30 & exit 0");
    client.spawn(service, command, STARTUP_TIMEOUT).await?;
    expect_event(client, "service start", |event| started(event, service)).await?;
    expect_event(client, "service exit", |event| {
        child_exited(event, service, 0)
    })
    .await?;

    let follow_up = generation(2)?;
    client
        .spawn(
            follow_up,
            ProcessCommand::new("/usr/bin/true"),
            STARTUP_TIMEOUT,
        )
        .await?;
    expect_event(client, "follow-up start", |event| started(event, follow_up)).await?;
    expect_event(client, "follow-up exit", |event| {
        child_exited(event, follow_up, 0)
    })
    .await?;
    Ok(())
}

/// An inherited-lifetime service backgrounds a grandchild that exits on its own.
///
/// Unlike a foreground service, the generation's group is not torn down, so the
/// grandchild is never killed: it runs briefly, exits, and the broker reaps it
/// through an ordinary wait rather than group cleanup. The main shell's exit and
/// the inherited-lifetime closure race through independent channels, so both
/// orders are accepted. A longer-lived owned service then keeps the broker alive
/// across the orphan's lifetime; because that service outlives the orphan, its
/// own exit is guaranteed to arrive only after the orphan has been reaped, so
/// observing it proves the broker survived the hygiene reap and emitted no stray
/// event for the orphan in between.
async fn self_exiting_orphan_is_hygiene(
    client: &mut ProcessBrokerClient,
) -> Result<(), Box<dyn Error>> {
    let service = generation(1)?;
    let mut command = ProcessCommand::new("/bin/sh");
    command.argument("-c").argument("(/bin/sleep 0.1) & exit 0");
    client
        .spawn_with_lifetime(service, command, STARTUP_TIMEOUT, None)
        .await?;
    expect_event(client, "service start", |event| started(event, service)).await?;
    expect_pair(
        client,
        "service exit and inherited lifetime closure",
        |event| child_exited(event, service, 0),
        |event| {
            matches!(
                event,
                ProcessBrokerEvent::LifetimeClosed { generation } if *generation == service
            )
        },
    )
    .await?;

    let keepalive = generation(2)?;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("0.5");
    client.spawn(keepalive, command, STARTUP_TIMEOUT).await?;
    expect_event(client, "keepalive start", |event| started(event, keepalive)).await?;
    expect_event(client, "keepalive exit", |event| {
        child_exited(event, keepalive, 0)
    })
    .await?;
    Ok(())
}

/// An orphan is adopted while a first service is still owned and running.
///
/// A resident service sleeps while a transient service spawns and orphans a
/// grandchild; the broker adopts and reaps that orphan with the resident still
/// in its ownership map. Terminating the resident afterwards must yield exactly
/// its own acknowledgement and terminal event, proving the adopted orphan never
/// perturbed a live service's bookkeeping.
async fn orphan_does_not_disturb_owned_service(
    client: &mut ProcessBrokerClient,
) -> Result<(), Box<dyn Error>> {
    let resident = generation(1)?;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("30");
    client.spawn(resident, command, STARTUP_TIMEOUT).await?;
    expect_event(client, "resident start", |event| started(event, resident)).await?;

    let transient = generation(2)?;
    let mut command = ProcessCommand::new("/bin/sh");
    command.argument("-c").argument("/bin/sleep 30 & exit 0");
    client.spawn(transient, command, STARTUP_TIMEOUT).await?;
    expect_event(client, "transient start", |event| started(event, transient)).await?;
    expect_event(client, "transient exit", |event| {
        child_exited(event, transient, 0)
    })
    .await?;

    client
        .signal(resident, BrokerSignalScope::Group, ProcessSignal::Terminate)
        .await?;
    expect_event(client, "resident signal acknowledgement", |event| {
        matches!(
            event,
            ProcessBrokerEvent::SignalDelivered { generation } if *generation == resident
        )
    })
    .await?;
    expect_event(client, "resident termination", |event| {
        child_signaled(event, resident)
    })
    .await?;
    Ok(())
}

/// Await the next broker event under a hard deadline.
///
/// The bounded timeout keeps a wedged broker from hanging the suite instead of
/// blocking forever on a broker that died while reaping an adopted orphan.
async fn receive_event(
    client: &mut ProcessBrokerClient,
    description: &str,
) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {description}"),
            )
        })??;
    Ok(event)
}

/// Await the next event and assert its shape.
///
/// A mismatch is reported with its description so a stray adopted-orphan event
/// fails loudly at the exact step that expected a workload event.
async fn expect_event(
    client: &mut ProcessBrokerClient,
    description: &str,
    predicate: impl FnOnce(&ProcessBrokerEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    let event = receive_event(client, description).await?;
    if predicate(&event) {
        Ok(())
    } else {
        Err(io::Error::other(format!("expected {description}, received {event:?}")).into())
    }
}

/// Await exactly two events that satisfy two predicates in either order.
///
/// The broker delivers a main-child exit and an inherited-lifetime closure
/// through independent paths that may interleave, so this accepts one event for
/// each predicate regardless of arrival order while still rejecting any third,
/// duplicated, or unrelated event.
async fn expect_pair(
    client: &mut ProcessBrokerClient,
    description: &str,
    first: impl Fn(&ProcessBrokerEvent) -> bool,
    second: impl Fn(&ProcessBrokerEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    let mut need_first = true;
    let mut need_second = true;
    for _ in 0..2 {
        let event = receive_event(client, description).await?;
        if need_first && first(&event) {
            need_first = false;
        } else if need_second && second(&event) {
            need_second = false;
        } else {
            return Err(
                io::Error::other(format!("expected {description}, received {event:?}")).into(),
            );
        }
    }
    Ok(())
}

fn generation(value: u64) -> Result<Generation, Box<dyn Error>> {
    Generation::new(value)
        .ok_or_else(|| io::Error::other("test generation must be nonzero"))
        .map_err(Into::into)
}

fn started(event: &ProcessBrokerEvent, expected: Generation) -> bool {
    matches!(
        event,
        ProcessBrokerEvent::Started { generation, .. } if *generation == expected
    )
}

fn child_exited(event: &ProcessBrokerEvent, expected: Generation, expected_code: u8) -> bool {
    matches!(
        event,
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Exited { code, .. },
        } if *generation == expected && *code == expected_code
    )
}

fn child_signaled(event: &ProcessBrokerEvent, expected: Generation) -> bool {
    matches!(
        event,
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Signaled { .. },
        } if *generation == expected
    )
}
