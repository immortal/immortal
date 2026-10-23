//! Fresh-process contract for draining groups before the broker exits.
//!
//! An ordered broker shutdown may report completion only once every process
//! group the broker killed or owns has no member left. The broker signals a
//! group only while its generation's armed guard, or an unreaped member child,
//! pins its ID, so a descriptor group is terminated and then killed, while a
//! tracked group whose generation already completed is awaited without a
//! signal and makes shutdown fail closed while a member keeps running.
//! Otherwise a still-exiting member reparents to the supervisor after the
//! broker exits. Supervisor-loss cleanup must likewise run a descriptor
//! service's stop command even while an earlier killed group cannot drain.
//! Each scenario forks its own broker, and a residual check proves the broker
//! reaped everything it owned instead of handing it here.
//!
//! The escaped-holder scenarios re-execute this binary twice: once as the
//! service leader inside the generation's group, and once as a holder in its
//! own group whose unreaped child stays a member of the generation's group
//! after the broker killed it. Every fixture is bounded by a hard self-timeout
//! and is released through a marker file on both success and failure paths.

#[path = "support/bounded_shell.rs"]
mod bounded_shell;
#[path = "support/broker_guard.rs"]
mod broker_guard;
#[path = "support/group_guard.rs"]
mod group_guard;
#[path = "support/release_directory.rs"]
mod release_directory;
#[path = "support/residual_children.rs"]
mod residual_children;
#[path = "support/subtree_guard.rs"]
mod subtree_guard;

use std::{
    env,
    error::Error,
    ffi::OsString,
    fs, io,
    os::unix::process::CommandExt,
    path::{Path, PathBuf},
    process::Command,
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        BrokerLifetimePlan, BrokerSignalScope, ChildEvent, ProcessBrokerClient, ProcessBrokerEvent,
        ProcessCommand, ProcessEnvironment, ProcessGroupId, ProcessId, ProcessSignal,
        process_group_exists, reap_any_event, reap_event, signal_child, start_process_broker,
        start_process_broker_with_lifetime,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;
use crate::group_guard::GroupGuard;
use crate::residual_children::assert_no_residual_children;

/// The broker exit status when supervisor-loss cleanup cannot complete.
const BROKER_EXIT_SOFTWARE: u8 = 70;
const FIXTURE_DIR: &str = "IMMORTAL_RETIRING_FIXTURE_DIR";
const FIXTURE_GROUP: &str = "IMMORTAL_RETIRING_FIXTURE_GROUP";
/// Keeps the leader alive after its holder is ready, until the group is killed.
const FIXTURE_HOLD: &str = "IMMORTAL_RETIRING_FIXTURE_HOLD";
const FIXTURE_MODE: &str = "IMMORTAL_RETIRING_FIXTURE_MODE";
/// Covers the broker's two-phase shutdown (two plus three seconds) with margin.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const FIXTURE_TIMEOUT: Duration = Duration::from_secs(10);
/// The holder keeps its member unreaped at most this long if never released.
const HOLDER_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Just under the broker's two-second terminate grace; a member which ignores
/// `SIGTERM` cannot be gone sooner unless the broker skipped the grace.
const MINIMUM_ESCALATION_DELAY: Duration = Duration::from_millis(1_900);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> Result<(), Box<dyn Error>> {
    match env::var(FIXTURE_MODE).as_deref() {
        Ok("leader") => return leader_fixture(),
        Ok("holder") => return holder_fixture(),
        _ => {}
    }
    let _orphans = subtree_guard::SubtreeGuard;
    killed_group_held_by_escaped_parent_fails_closed()?;
    supervisor_group_kill_is_awaited_after_lifetime_closure()?;
    blocked_group_does_not_skip_the_stop_hook()?;
    descriptor_group_receives_term_before_shutdown()?;
    descriptor_group_ignoring_term_is_killed()?;
    closed_lifetime_group_is_awaited_after_its_launcher_exits()?;
    Ok(())
}

/// A killed group which cannot drain keeps shutdown from reporting completion.
///
/// The service leader exits after a holder outside its group has placed an
/// unreaped member inside it, so the group the broker kills on the leader's
/// reap stays occupied by a zombie only the holder can reap. Shutdown must fail
/// closed instead of completing; once the holder is released and the group
/// drains, a second shutdown completes.
fn killed_group_held_by_escaped_parent_fails_closed() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("escaped-holder")?;
    let endpoint = start_process_broker()?;
    let mut reaper = ScenarioReaper::new(endpoint.process(), &paths);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let holder = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect(&mut client, "broker readiness", |event| {
            matches!(event, ProcessBrokerEvent::Ready)
        })
        .await?;
        let group = start_leader(&mut client, &paths).await?;
        let holder = read_holder(&paths)?;
        if process_group_exists(group)? {
            client.shutdown().await?;
            expect(&mut client, "fail-closed shutdown", |event| {
                matches!(event, ProcessBrokerEvent::ShutdownFailed { .. })
            })
            .await?;
            paths.release()?;
            wait_until_absent(group, "released holder did not drain the killed group")?;
            // The holder exits only after reaping its member, so wait until it
            // is gone too; otherwise the broker could exit first and hand it here.
            wait_until_absent(holder_group(holder)?, "released holder did not exit")?;
        }
        client.shutdown().await?;
        expect(&mut client, "shutdown completion", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownComplete)
        })
        .await?;
        Ok::<ProcessId, Box<dyn Error>>(holder)
    })?;
    drop(runtime);
    reaper.wait_for_broker(holder, 0)?;
    reaper.release_holder(holder)?;
    assert_no_residual_children("escaped-holder scenario")?;
    Ok(())
}

/// A group the supervisor kills is awaited even after its generation is gone.
///
/// The tracked leader closes its lifetime descriptor before it execs, so the
/// broker forgets the generation as soon as the supervisor's group kill ends
/// the leader. The escaped holder keeps a killed member's zombie in the group,
/// so the shutdown that follows must fail closed rather than complete with the
/// killed group still occupied; once the holder is released it completes.
fn supervisor_group_kill_is_awaited_after_lifetime_closure() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("supervisor-kill")?;
    let endpoint = start_process_broker()?;
    let mut reaper = ScenarioReaper::new(endpoint.process(), &paths);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let holder = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect(&mut client, "broker readiness", |event| {
            matches!(event, ProcessBrokerEvent::Ready)
        })
        .await?;
        let service = Generation::FIRST;
        let mut environment = ProcessEnvironment::new();
        environment.insert(OsString::from(FIXTURE_MODE), OsString::from("leader"));
        environment.insert(OsString::from(FIXTURE_HOLD), OsString::from("1"));
        environment.insert(
            OsString::from(FIXTURE_DIR),
            paths.base.as_os_str().to_os_string(),
        );
        environment.insert(OsString::from("LEADER"), env::current_exe()?.into_os_string());
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument("exec 4>&-; exec \"$LEADER\"")
            .environment(environment);
        client
            .spawn_with_lifetime(service, command, STARTUP_TIMEOUT, None)
            .await?;
        let group = match receive(&mut client, "leader start").await? {
            ProcessBrokerEvent::Started {
                generation, group, ..
            } if generation == service => group,
            event => {
                return Err(io::Error::other(format!(
                    "expected leader start, received {event:?}"
                ))
                .into());
            }
        };
        fs::write(paths.group(), group.get().to_string())?;
        expect(&mut client, "lifetime closure", |event| {
            matches!(event, ProcessBrokerEvent::LifetimeClosed { generation } if *generation == service)
        })
        .await?;
        let holder = read_holder(&paths)?;
        client
            .signal(service, BrokerSignalScope::Group, ProcessSignal::Kill)
            .await?;
        let (mut delivered, mut leader_exited) = (false, false);
        while !(delivered && leader_exited) {
            match receive(&mut client, "group kill outcome").await? {
                ProcessBrokerEvent::SignalDelivered { generation } if generation == service => {
                    delivered = true;
                }
                ProcessBrokerEvent::Child { generation, event }
                    if generation == service && event.is_terminal() =>
                {
                    leader_exited = true;
                }
                event => {
                    return Err(io::Error::other(format!(
                        "expected the group kill outcome, received {event:?}"
                    ))
                    .into());
                }
            }
        }
        if process_group_exists(group)? {
            client.shutdown().await?;
            expect(&mut client, "fail-closed shutdown", |event| {
                matches!(event, ProcessBrokerEvent::ShutdownFailed { .. })
            })
            .await?;
            paths.release()?;
            wait_until_absent(group, "released holder did not drain the killed group")?;
            wait_until_absent(holder_group(holder)?, "released holder did not exit")?;
        }
        client.shutdown().await?;
        expect(&mut client, "shutdown completion", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownComplete)
        })
        .await?;
        Ok::<ProcessId, Box<dyn Error>>(holder)
    })?;
    drop(runtime);
    reaper.wait_for_broker(holder, 0)?;
    reaper.release_holder(holder)?;
    assert_no_residual_children("supervisor group kill scenario")?;
    Ok(())
}

/// A killed group which cannot drain must not keep the stop hook from running.
///
/// After supervisor loss a descriptor-tracked service still needs its stop
/// command while an earlier killed group stays occupied by a zombie only the
/// escaped holder can reap. The broker must run the hook and let the service
/// stop first, and only then fail closed because the killed group never drained.
fn blocked_group_does_not_skip_the_stop_hook() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("loss-hook")?;
    let endpoint = start_process_broker_with_lifetime(stop_plan(&paths)?)?;
    let mut reaper = ScenarioReaper::new(endpoint.process(), &paths);
    let mut service = None;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let holder = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect(&mut client, "broker readiness", |event| {
            matches!(event, ProcessBrokerEvent::Ready)
        })
        .await?;
        start_leader(&mut client, &paths).await?;
        let holder = read_holder(&paths)?;
        service = Some(GroupGuard::released(
            start_stoppable_service(&mut client, &paths).await?,
            vec![paths.stop()],
            EVENT_TIMEOUT,
            POLL_INTERVAL,
        ));
        drop(client);
        Ok::<ProcessId, Box<dyn Error>>(holder)
    })?;
    drop(runtime);
    reaper.wait_for_broker(holder, BROKER_EXIT_SOFTWARE)?;
    if !paths.done().exists() {
        return Err(io::Error::other(
            "supervisor-loss cleanup failed closed without running the stop hook",
        )
        .into());
    }
    if let Some(service) = &mut service {
        service.disarm();
    }
    reaper.release_holder(holder)?;
    assert_no_residual_children("blocked-group supervisor loss")?;
    Ok(())
}

/// Start a descriptor-tracked service which stops once the stop hook runs.
async fn start_stoppable_service(
    client: &mut ProcessBrokerClient,
    paths: &FixturePaths,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    let service = Generation::new(2).ok_or("invalid service generation")?;
    let mut environment = ProcessEnvironment::new();
    environment.insert(OsString::from("STOP"), paths.stop().into_os_string());
    environment.insert(OsString::from("DONE"), paths.done().into_os_string());
    let mut command = ProcessCommand::new("/bin/sh");
    command
        .argument("-c")
        .argument(format!(
            "{}(wait_for \"$STOP\"; printf stopped > \"$DONE\") & exit 0",
            bounded_shell::PREFIX
        ))
        .environment(environment);
    client
        .spawn_with_lifetime(service, command, STARTUP_TIMEOUT, None)
        .await?;
    let group = match receive(client, "service start").await? {
        ProcessBrokerEvent::Started {
            generation, group, ..
        } if generation == service => group,
        event => {
            return Err(
                io::Error::other(format!("expected service start, received {event:?}")).into(),
            );
        }
    };
    paths.record(group);
    expect(client, "service launcher exit", |event| {
        matches!(
            event,
            ProcessBrokerEvent::Child {
                generation,
                event: ChildEvent::Exited { code: 0, .. },
            } if *generation == service
        )
    })
    .await?;
    Ok(group)
}

/// The supervisor-loss stop command: let the tracked service stop.
fn stop_plan(paths: &FixturePaths) -> io::Result<BrokerLifetimePlan> {
    let mut environment = ProcessEnvironment::new();
    environment.insert(OsString::from("STOP"), paths.stop().into_os_string());
    let mut stop = ProcessCommand::new("/bin/sh");
    stop.argument("-c")
        .argument(": > \"$STOP\"")
        .environment(environment);
    BrokerLifetimePlan::new(stop, STARTUP_TIMEOUT, EVENT_TIMEOUT)
}

/// Start the service leader and wait until it exits with its group occupied.
async fn start_leader(
    client: &mut ProcessBrokerClient,
    paths: &FixturePaths,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    let service = Generation::FIRST;
    let mut environment = ProcessEnvironment::new();
    environment.insert(OsString::from(FIXTURE_MODE), OsString::from("leader"));
    environment.insert(
        OsString::from(FIXTURE_DIR),
        paths.base.as_os_str().to_os_string(),
    );
    let mut command = ProcessCommand::new(env::current_exe()?);
    command.environment(environment);
    client.spawn(service, command, STARTUP_TIMEOUT).await?;
    let group = match receive(client, "leader start").await? {
        ProcessBrokerEvent::Started {
            generation, group, ..
        } if generation == service => group,
        event => {
            return Err(
                io::Error::other(format!("expected leader start, received {event:?}")).into(),
            );
        }
    };
    fs::write(paths.group(), group.get().to_string())?;
    paths.record(group);
    expect(client, "leader exit", |event| {
        matches!(
            event,
            ProcessBrokerEvent::Child {
                generation,
                event: ChildEvent::Exited { code: 0, .. },
            } if *generation == service
        )
    })
    .await?;
    Ok(group)
}

/// Run as the service leader: hand a holder outside the group our group ID.
///
/// The holder places its member in this group and reports ready; the leader
/// then exits so the broker reaps it and kills the group. The holder is never
/// waited for here: it must outlive this leader, and whichever process inherits
/// it as a child reaps it.
fn leader_fixture() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::from_environment()?;
    let group = wait_for_contents(&paths.group(), FIXTURE_TIMEOUT)?;
    Command::new(env::current_exe()?)
        .env(FIXTURE_MODE, "holder")
        .env(FIXTURE_DIR, &paths.base)
        .env(FIXTURE_GROUP, group.trim())
        .process_group(0)
        .spawn()?;
    wait_for_contents(&paths.ready(), FIXTURE_TIMEOUT)?;
    if env::var_os(FIXTURE_HOLD).is_some() {
        // Stay in the group until the scenario kills it, within the deadline.
        thread::sleep(FIXTURE_TIMEOUT);
    }
    Ok(())
}

/// Run as the holder: keep one member of the leader's group unreaped.
///
/// The member joins the leader's group while the holder stays in its own, so
/// once the broker kills that group the member's zombie can be reaped only by
/// the holder. The holder reaps it when released or at its hard deadline.
fn holder_fixture() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::from_environment()?;
    let group: i32 = env::var(FIXTURE_GROUP)?.parse()?;
    let mut member = Command::new("/bin/sleep")
        .arg("30")
        .process_group(group)
        .spawn()?;
    fs::write(paths.ready(), std::process::id().to_string())?;
    let _ = wait_for_contents(&paths.release_path(), HOLDER_TIMEOUT);
    let _ = member.kill();
    member.wait()?;
    Ok(())
}

/// A background member that records `SIGTERM` and exits on it.
const TERMINATING_MEMBER: &str = concat!(
    "(trap \"printf term > \\\"$MARK\\\"; exit 0\" TERM; printf ready > \"$READY\"; ",
    "wait_for \"$STOP\") & ",
);

/// A background member that ignores `SIGTERM`.
const TERM_IGNORING_MEMBER: &str = concat!(
    "(trap \"\" TERM; printf ready > \"$READY\"; ",
    "wait_for \"$STOP\") & ",
);

/// Descriptor-tracked members are asked to terminate before shutdown completes.
///
/// The launcher exits while a background member keeps the lifetime descriptor
/// open, so only the generation's armed guard pins the group. Shutdown must
/// deliver `SIGTERM` to that group and wait for it to drain, rather than
/// complete at once or leave the group to a blind kill after the broker exits.
fn descriptor_group_receives_term_before_shutdown() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("descriptor-term")?;
    let elapsed = run_descriptor_scenario(&paths, TERMINATING_MEMBER)?;
    let mark = fs::read_to_string(paths.mark()).unwrap_or_default();
    if mark != "term" {
        return Err(io::Error::other(format!(
            "descriptor member did not receive SIGTERM before shutdown completed after {elapsed:?}"
        ))
        .into());
    }
    Ok(())
}

/// A descriptor-tracked member that ignores `SIGTERM` is killed after the grace.
fn descriptor_group_ignoring_term_is_killed() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("descriptor-kill")?;
    let elapsed = run_descriptor_scenario(&paths, TERM_IGNORING_MEMBER)?;
    if elapsed < MINIMUM_ESCALATION_DELAY {
        return Err(io::Error::other(format!(
            "shutdown completed after {elapsed:?}, before the terminate grace elapsed"
        ))
        .into());
    }
    Ok(())
}

/// A tracked group whose lifetime closed while its launcher ran is awaited
/// once the launcher exits.
///
/// Shutdown's `SIGTERM` reaches the member while the generation still pins the
/// group, and the member ignores it. The launcher's exit completes the
/// generation, which retires the group and disarms its guard, so the group is
/// no longer the broker's to signal: shutdown must not escalate to a kill, and
/// fails closed instead of completing while the member runs. The member marks its trap installed before the launcher closes the
/// descriptor, so the observed closure proves the member ignores `SIGTERM`.
fn closed_lifetime_group_is_awaited_after_its_launcher_exits() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("closed-lifetime")?;
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let mut member = None;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect(&mut client, "broker readiness", |event| {
            matches!(event, ProcessBrokerEvent::Ready)
        })
        .await?;
        let service = Generation::FIRST;
        let mut environment = ProcessEnvironment::new();
        environment.insert(OsString::from("MARK"), paths.mark().into_os_string());
        environment.insert(OsString::from("STOP"), paths.stop().into_os_string());
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument(format!("{}{}", bounded_shell::PREFIX, concat!(
                "(exec 4>&-; trap \"\" TERM; : > \"$MARK\"; wait_for \"$STOP\") & ",
                // Close the descriptor permanently before exec: macOS's bash
                // 3.2 saves it for `exec sleep 30 4>&-`, and sleep inherits
                // that copy, so the lifetime never closes.
                "wait_for \"$MARK\" 4>&-; exec 4>&-; exec sleep 30",
            )))
            .environment(environment);
        client
            .spawn_with_lifetime(service, command, STARTUP_TIMEOUT, None)
            .await?;
        let guard = match receive(&mut client, "service start").await? {
            ProcessBrokerEvent::Started {
                generation, group, ..
            } if generation == service => {
                paths.record(group);
                member.insert(GroupGuard::released(group, vec![paths.stop()], EVENT_TIMEOUT, POLL_INTERVAL))
            }
            event => {
                return Err(io::Error::other(format!(
                    "expected service start, received {event:?}"
                ))
                .into());
            }
        };
        expect(&mut client, "lifetime closure", |event| {
            matches!(event, ProcessBrokerEvent::LifetimeClosed { generation } if *generation == service)
        })
        .await?;
        client.shutdown().await?;
        loop {
            match receive(&mut client, "fail-closed shutdown").await? {
                ProcessBrokerEvent::ShutdownFailed { .. } => break,
                ProcessBrokerEvent::Child { generation, .. } if generation == service => {}
                event => {
                    return Err(io::Error::other(format!(
                        "expected a fail-closed shutdown, received {event:?}"
                    ))
                    .into());
                }
            }
        }
        guard.kill_and_wait()?;
        client.shutdown().await?;
        expect(&mut client, "shutdown completion", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownComplete)
        })
        .await?;
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(runtime);
    broker.wait()?;
    assert_no_residual_children("closed-lifetime scenario")?;
    Ok(())
}

/// Spawn a lifetime-tracked service, shut down after its launcher exits, and
/// return how long the shutdown took to complete.
///
/// `member` starts the background member, which must write `$READY` once its
/// `SIGTERM` disposition is installed. The launcher exits only after that, so
/// its exit event proves shutdown's `SIGTERM` cannot overtake the trap.
fn run_descriptor_scenario(paths: &FixturePaths, member: &str) -> Result<Duration, Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let mut cleanup = None;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let elapsed = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect(&mut client, "broker readiness", |event| {
            matches!(event, ProcessBrokerEvent::Ready)
        })
        .await?;
        let group = start_descriptor_service(&mut client, paths, member).await?;
        cleanup = Some(GroupGuard::released(
            group,
            vec![paths.stop()],
            EVENT_TIMEOUT,
            POLL_INTERVAL,
        ));
        let started = Instant::now();
        client.shutdown().await?;
        // The member's exit closes its descriptor, which shutdown reports
        // before it completes.
        expect_completion_after_closure(&mut client).await?;
        Ok::<Duration, Box<dyn Error>>(started.elapsed())
    })?;
    drop(runtime);
    broker.wait()?;
    assert_no_residual_children("descriptor scenario")?;
    // A completed shutdown drained the group, so nothing is left to kill.
    if let Some(cleanup) = &mut cleanup {
        cleanup.disarm();
    }
    Ok(elapsed)
}

/// Start the descriptor service and wait until its launcher has exited.
async fn start_descriptor_service(
    client: &mut ProcessBrokerClient,
    paths: &FixturePaths,
    member: &str,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    let service = Generation::FIRST;
    let mut environment = ProcessEnvironment::new();
    environment.insert(OsString::from("MARK"), paths.mark().into_os_string());
    environment.insert(OsString::from("READY"), paths.ready().into_os_string());
    environment.insert(OsString::from("STOP"), paths.stop().into_os_string());
    let script = format!(
        "{}{member}wait_for \"$READY\"; exit 0",
        bounded_shell::PREFIX
    );
    let mut command = ProcessCommand::new("/bin/sh");
    command
        .argument("-c")
        .argument(script)
        .environment(environment);
    client
        .spawn_with_lifetime(service, command, STARTUP_TIMEOUT, None)
        .await?;
    let group = match receive(client, "descriptor service start").await? {
        ProcessBrokerEvent::Started {
            generation, group, ..
        } if generation == service => group,
        event => {
            return Err(io::Error::other(format!(
                "expected descriptor service start, received {event:?}"
            ))
            .into());
        }
    };
    paths.record(group);
    expect(client, "descriptor launcher exit", |event| {
        matches!(
            event,
            ProcessBrokerEvent::Child {
                generation,
                event: ChildEvent::Exited { code: 0, .. },
            } if *generation == service
        )
    })
    .await?;
    Ok(group)
}

/// Await shutdown completion after the descriptor member exited.
///
/// The member's exit closes its descriptor, so the broker may report that
/// closure before it answers the shutdown.
async fn expect_completion_after_closure(
    client: &mut ProcessBrokerClient,
) -> Result<(), Box<dyn Error>> {
    loop {
        match receive(client, "shutdown completion").await? {
            ProcessBrokerEvent::ShutdownComplete => return Ok(()),
            ProcessBrokerEvent::LifetimeClosed { generation }
                if generation == Generation::FIRST => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected shutdown completion, received {event:?}"
                ))
                .into());
            }
        }
    }
}

async fn receive(
    client: &mut ProcessBrokerClient,
    description: &str,
) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    tokio::time::timeout(EVENT_TIMEOUT, client.next_event())
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {description}"),
            )
        })?
        .map_err(Into::into)
}

async fn expect(
    client: &mut ProcessBrokerClient,
    description: &str,
    predicate: impl FnOnce(&ProcessBrokerEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    let event = receive(client, description).await?;
    if predicate(&event) {
        Ok(())
    } else {
        Err(io::Error::other(format!("expected {description}, received {event:?}")).into())
    }
}

fn read_holder(paths: &FixturePaths) -> Result<ProcessId, Box<dyn Error>> {
    let raw: i32 = wait_for_contents(&paths.ready(), FIXTURE_TIMEOUT)?
        .trim()
        .parse()?;
    ProcessId::new(raw).ok_or_else(|| "holder reported an invalid process ID".into())
}

/// The holder leads its own process group, whose ID is the holder's PID.
fn holder_group(holder: ProcessId) -> io::Result<ProcessGroupId> {
    ProcessGroupId::try_from(holder.get())
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

fn wait_until_absent(group: ProcessGroupId, failure: &str) -> io::Result<()> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while process_group_exists(group)? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, failure));
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// Wait until `path` exists with nonempty contents, and return them.
fn wait_for_contents(path: &Path, timeout: Duration) -> io::Result<String> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(contents) = fs::read_to_string(path)
            && !contents.is_empty()
        {
            return Ok(contents);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {}", path.display()),
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Marker files shared by the contract and its re-executed fixtures.
struct FixturePaths {
    base: PathBuf,
    cleanup: Option<release_directory::ReleaseDirectory>,
}

impl FixturePaths {
    fn new(name: &str) -> io::Result<Self> {
        let base = env::temp_dir().join(format!("immortal-retiring-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir(&base)?;
        let cleanup = Some(release_directory::ReleaseDirectory::new(
            base.clone(),
            &["release", "stop"],
        ));
        Ok(Self { base, cleanup })
    }

    fn from_environment() -> io::Result<Self> {
        let base = env::var_os(FIXTURE_DIR)
            .map(PathBuf::from)
            .ok_or_else(|| io::Error::other("fixture directory is not set"))?;
        Ok(Self {
            base,
            cleanup: None,
        })
    }

    fn record(&self, group: ProcessGroupId) {
        if let Some(cleanup) = &self.cleanup {
            cleanup.record(group);
        }
    }

    fn group(&self) -> PathBuf {
        self.base.join("group")
    }

    fn mark(&self) -> PathBuf {
        self.base.join("mark")
    }

    fn ready(&self) -> PathBuf {
        self.base.join("ready")
    }

    fn release_path(&self) -> PathBuf {
        self.base.join("release")
    }

    fn stop(&self) -> PathBuf {
        self.base.join("stop")
    }

    fn done(&self) -> PathBuf {
        self.base.join("done")
    }

    fn release(&self) -> io::Result<()> {
        fs::write(self.release_path(), "release")
    }
}

impl Drop for FixturePaths {
    fn drop(&mut self) {
        // A holder can report before the scenario reaches read_holder.
        if let Ok(raw) = fs::read_to_string(self.ready())
            && let Ok(pid) = raw.trim().parse::<i32>()
            && let Ok(group) = ProcessGroupId::try_from(pid)
        {
            self.record(group);
        }
        drop(self.cleanup.take());
    }
}

/// Reaps the broker, and releases and awaits the holder.
///
/// The holder is the leader's child, so once the leader exits it reparents to
/// the broker, and to this process if the broker exits first; on macOS launchd
/// adopts it instead. Dropping an unfinished reaper kills an unreaped broker,
/// releases the holder, and waits until the holder is gone and every child of
/// this process is reaped, so a failing scenario leaves nothing behind.
struct ScenarioReaper<'paths> {
    broker: ProcessId,
    broker_reaped: bool,
    finished: bool,
    paths: &'paths FixturePaths,
}

impl<'paths> ScenarioReaper<'paths> {
    const fn new(broker: ProcessId, paths: &'paths FixturePaths) -> Self {
        Self {
            broker,
            broker_reaped: false,
            finished: false,
            paths,
        }
    }

    /// Reap the broker and require exit status `expected`.
    fn wait_for_broker(&mut self, holder: ProcessId, expected: u8) -> io::Result<()> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match reap_event(self.broker)? {
                Some(event) if event.pid() == self.broker && event.is_terminal() => {
                    self.broker_reaped = true;
                    return match event {
                        ChildEvent::Exited { code, .. } if code == expected => Ok(()),
                        event => Err(io::Error::other(format!(
                            "broker terminated with {event:?}, expected status {expected}"
                        ))),
                    };
                }
                Some(event) if event.pid() == holder => {}
                Some(event) => {
                    return Err(io::Error::other(format!(
                        "reaped unexpected child event {event:?}"
                    )));
                }
                None => {}
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for the process broker",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    /// Release the holder and wait until it has exited and been reaped.
    fn release_holder(&mut self, holder: ProcessId) -> io::Result<()> {
        self.paths.release()?;
        let group = holder_group(holder)?;
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == holder => {}
                Ok(Some(event)) => {
                    return Err(io::Error::other(format!(
                        "reaped unexpected child event {event:?}"
                    )));
                }
                Ok(None) => {}
                Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {}
                Err(error) => return Err(error),
            }
            if !process_group_exists(group)? {
                self.finished = true;
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "released holder did not exit",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for ScenarioReaper<'_> {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if !self.broker_reaped {
            let _ = signal_child(self.broker, ProcessSignal::Kill);
        }
        let _ = self.paths.release();
        let holder = fs::read_to_string(self.paths.ready())
            .ok()
            .and_then(|raw| raw.trim().parse::<i32>().ok())
            .and_then(|raw| ProcessGroupId::try_from(raw).ok());
        let deadline = Instant::now() + HOLDER_TIMEOUT;
        while Instant::now() < deadline {
            let children_left = reap_any_event().is_ok();
            let holder_left =
                holder.is_some_and(|group| process_group_exists(group).unwrap_or(false));
            if !children_left && !holder_left {
                return;
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}
