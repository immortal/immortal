//! Fresh-process contract for groups released when a tracked generation completes.
//!
//! A lifetime-tracked generation completes once its launcher is reaped and
//! every holder has closed descriptor 4, in either order. Completion ends the
//! generation by its own contract and disarms its guard, releasing the guard's
//! pin on the group's ID, so the broker must never signal the group; it
//! retires it instead, and ordered shutdown must wait for any member still in
//! it, failing closed while one keeps running. Each scenario drives
//! one completion order through file handshakes rather than delays, uses a
//! `SIGTERM` trap to prove the member was never signalled, and checks that
//! nothing reparented to this process.
//!
//! The daemon scenario re-executes this binary twice: as the launcher, and as
//! a daemon the launcher starts in its own process group. The daemon keeps the
//! lifetime descriptor open after the reserved group has emptied, the usual
//! shape of a service that calls `setsid`, so only the descriptor's closure may
//! complete the generation. The daemon has a hard self-timeout and is released
//! through a marker file on success and failure paths.

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
        ChildEvent, ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand, ProcessEnvironment,
        ProcessGroupId, process_group_exists, start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;
use crate::group_guard::GroupGuard;
use crate::residual_children::assert_no_residual_children;

/// Covers the broker's two-phase shutdown (two plus three seconds) with margin.
const EVENT_TIMEOUT: Duration = Duration::from_secs(10);
const FIXTURE_DIR: &str = "IMMORTAL_LIFETIME_FIXTURE_DIR";
const FIXTURE_MODE: &str = "IMMORTAL_LIFETIME_FIXTURE_MODE";
/// The daemon exits on its own after this long if it is never released.
const DAEMON_TIMEOUT: Duration = Duration::from_secs(20);
/// How long the exiting member keeps running after shutdown was requested; a
/// broker that completes sooner did not wait for it.
const MEMBER_RUN_AFTER_SHUTDOWN: Duration = Duration::from_millis(400);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

/// The member closes descriptor 4, then the launcher closes it and waits for
/// `$GO`, so the lifetime closes while the launcher still runs.
const CLOSURE_FIRST: &str = concat!(
    "(exec 4>&-; trap \"printf term > \\\"$MARK\\\"\" TERM; printf ready > \"$READY\"; ",
    "sleep \"${MEMBER_DELAY:-0}\"; wait_for \"$FINISH\") & ",
    "wait_for \"$READY\" 4>&-; exec 4>&-; ",
    "wait_for \"$GO\"; exit \"${LAUNCHER_STATUS:-0}\"",
);
/// The member keeps descriptor 4 until `$CLOSE` appears; the launcher exits as
/// soon as the member is ready, so it is reaped before the lifetime closes.
const LAUNCHER_FIRST: &str = concat!(
    "(trap \"printf term > \\\"$MARK\\\"\" TERM; printf ready > \"$READY\"; ",
    "wait_for \"$CLOSE\"; exec 4>&-; ",
    "wait_for \"$FINISH\") & ",
    "wait_for \"$READY\"; exit 0",
);

fn main() -> Result<(), Box<dyn Error>> {
    match env::var(FIXTURE_MODE).as_deref() {
        Ok("launcher") => return launcher_fixture(),
        Ok("daemon") => return daemon_fixture(),
        _ => {}
    }
    let _orphans = subtree_guard::SubtreeGuard;
    failed_completion_releases_delayed_member()?;
    if env::args().any(|arg| arg == "--failure-only") {
        return Ok(());
    }
    a_daemon_that_left_its_group_keeps_shutdown_waiting()?;
    lifetime_completion_after_closure_awaits_a_running_member()?;
    lifetime_completion_after_launcher_reap_awaits_a_running_member()?;
    lifetime_completion_drains_an_exiting_member_before_shutdown()?;
    Ok(())
}

/// Completion through the launcher's reap, after the lifetime already closed,
/// retires the group: shutdown fails closed while the member runs and never
/// signals it, then completes once the member is gone.
fn lifetime_completion_after_closure_awaits_a_running_member() -> Result<(), Box<dyn Error>> {
    run_leftover_scenario("closure-first", CompletionOrder::ClosureFirst)
}

/// Completion through descriptor closure, after the launcher was already
/// reaped, retires the group with the same fail-closed outcome.
fn lifetime_completion_after_launcher_reap_awaits_a_running_member() -> Result<(), Box<dyn Error>> {
    run_leftover_scenario("launcher-first", CompletionOrder::LauncherFirst)
}

/// A member still exiting at completion is awaited: shutdown completes only
/// after it exits, without ever signalling it, and leaves nothing behind.
fn lifetime_completion_drains_an_exiting_member_before_shutdown() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("exiting-member")?;
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let mut member = None;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let elapsed = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect_ready(&mut client).await?;
        complete_generation(
            &mut client,
            &paths,
            CompletionOrder::ClosureFirst,
            &mut member,
        )
        .await?;
        let started = Instant::now();
        client.shutdown().await?;
        // Let the member finish only after shutdown has started waiting.
        let finish = paths.finish();
        let releaser = thread::spawn(move || {
            thread::sleep(MEMBER_RUN_AFTER_SHUTDOWN);
            fs::write(finish, "finish")
        });
        let event = receive(&mut client, "shutdown completion").await?;
        releaser
            .join()
            .map_err(|_| io::Error::other("member releaser panicked"))??;
        if !matches!(event, ProcessBrokerEvent::ShutdownComplete) {
            return Err(io::Error::other(format!(
                "expected shutdown completion, received {event:?}"
            ))
            .into());
        }
        Ok::<Duration, Box<dyn Error>>(started.elapsed())
    })?;
    drop(runtime);
    broker.wait()?;
    assert_no_residual_children("exiting-member scenario")?;
    if let Some(member) = &mut member {
        member.disarm();
    }
    if elapsed < MEMBER_RUN_AFTER_SHUTDOWN {
        return Err(io::Error::other(format!(
            "shutdown completed after {elapsed:?}, while the released member still ran"
        ))
        .into());
    }
    paths.require_unsignalled()
}

/// An emptied reserved group never completes a generation whose descriptor is open.
///
/// The daemon left the reserved group but still holds the lifetime
/// descriptor, so the service is running by its own contract. Shutdown must
/// fail closed and keep the generation, so supervisor-loss cleanup could still
/// run its stop command, rather than release it because the group looks
/// empty. Once the daemon exits and the descriptor closes, shutdown completes.
fn a_daemon_that_left_its_group_keeps_shutdown_waiting() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new("left-group-daemon")?;
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let mut daemon = None;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect_ready(&mut client).await?;
        let service = Generation::FIRST;
        let mut environment = ProcessEnvironment::new();
        environment.insert(OsString::from(FIXTURE_MODE), OsString::from("launcher"));
        environment.insert(OsString::from(FIXTURE_DIR), paths.base.clone().into_os_string());
        let mut command = ProcessCommand::new(env::current_exe()?);
        command.environment(environment);
        client
            .spawn_with_lifetime(service, command, STARTUP_TIMEOUT, None)
            .await?;
        expect(&mut client, "launcher start", |event| {
            matches!(event, ProcessBrokerEvent::Started { generation, .. } if *generation == service)
        })
        .await?;
        expect(&mut client, "launcher exit", |event| {
            matches!(
                event,
                ProcessBrokerEvent::Child {
                    generation,
                    event: ChildEvent::Exited { code: 0, .. },
                } if *generation == service
            )
        })
        .await?;
        let daemon_group = ProcessGroupId::try_from(read_number(&paths.ready())?)?;
        paths.record(daemon_group);
        daemon = Some(GroupGuard::released(daemon_group, vec![paths.close()], EVENT_TIMEOUT, POLL_INTERVAL));
        client.shutdown().await?;
        expect(&mut client, "fail-closed shutdown", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownFailed { .. })
        })
        .await?;
        fs::write(paths.close(), "close")?;
        expect(&mut client, "lifetime closure", |event| {
            matches!(event, ProcessBrokerEvent::LifetimeClosed { generation } if *generation == service)
        })
        .await?;
        // The daemon is outside every group the broker owns, so wait until it
        // is gone before the broker may exit and hand it to this process.
        wait_until_absent(daemon_group)?;
        client.shutdown().await?;
        expect(&mut client, "shutdown completion", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownComplete)
        })
        .await?;
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(runtime);
    broker.wait()?;
    assert_no_residual_children("left-group daemon scenario")?;
    if let Some(daemon) = &mut daemon {
        daemon.disarm();
    }
    Ok(())
}

/// Run as the launcher: start the daemon in its own group, then exit.
fn launcher_fixture() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::from_environment()?;
    Command::new(env::current_exe()?)
        .env(FIXTURE_MODE, "daemon")
        .env(FIXTURE_DIR, &paths.base)
        .process_group(0)
        .spawn()?;
    read_number(&paths.ready())?;
    Ok(())
}

/// Run as the daemon: hold the inherited lifetime descriptor until released.
fn daemon_fixture() -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::from_environment()?;
    fs::write(paths.ready(), std::process::id().to_string())?;
    let deadline = Instant::now() + DAEMON_TIMEOUT;
    while !exists(&paths.close()) && Instant::now() < deadline {
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

/// Read a positive number a fixture writes, waiting for it within the bound.
fn read_number(path: &Path) -> Result<i32, Box<dyn Error>> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if let Ok(raw) = fs::read_to_string(path)
            && let Ok(number) = raw.trim().parse::<i32>()
        {
            return Ok(number);
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for {}", path.display()),
            )
            .into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_until_absent(group: ProcessGroupId) -> io::Result<()> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while process_group_exists(group)? {
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the released daemon did not exit",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum CompletionOrder {
    ClosureFirst,
    LauncherFirst,
    FailedClosure,
}

/// Astra's failure schedule: completion rejects exit 73 while a member has not
/// reached its first FINISH poll. Cleanup must be prompt, durable and complete.
fn failed_completion_releases_delayed_member() -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let result = run_leftover_scenario("failed-closure", CompletionOrder::FailedClosure);
    let error = result
        .err()
        .ok_or("injected launcher failure was accepted")?;
    if !error.to_string().contains("code: 73") {
        return Err(error);
    }
    assert_no_residual_children("failed completion")?;
    if started.elapsed() >= Duration::from_secs(5) {
        return Err(io::Error::other("cleanup missed the delayed member's release poll").into());
    }
    Ok(())
}

/// Complete a tracked generation in `order` with a member left running, then
/// require a fail-closed shutdown that never signals the member.
fn run_leftover_scenario(name: &str, order: CompletionOrder) -> Result<(), Box<dyn Error>> {
    let paths = FixturePaths::new(name)?;
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let mut member = None;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        expect_ready(&mut client).await?;
        complete_generation(&mut client, &paths, order, &mut member).await?;
        let guard = member.as_mut().ok_or("member cleanup owner missing")?;
        client.shutdown().await?;
        expect(&mut client, "fail-closed shutdown", |event| {
            matches!(event, ProcessBrokerEvent::ShutdownFailed { .. })
        })
        .await?;
        paths.require_unsignalled()?;
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
    assert_no_residual_children(name)?;
    Ok(())
}

/// Spawn the tracked service and drive it to completion in `order`.
async fn complete_generation(
    client: &mut ProcessBrokerClient,
    paths: &FixturePaths,
    order: CompletionOrder,
    member: &mut Option<GroupGuard>,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    let service = Generation::FIRST;
    let mut environment = ProcessEnvironment::new();
    for (name, path) in [
        ("READY", paths.ready()),
        ("MARK", paths.mark()),
        ("GO", paths.go()),
        ("CLOSE", paths.close()),
        ("FINISH", paths.finish()),
    ] {
        environment.insert(OsString::from(name), path.into_os_string());
    }
    if matches!(order, CompletionOrder::FailedClosure) {
        environment.insert(OsString::from("LAUNCHER_STATUS"), OsString::from("73"));
        environment.insert(OsString::from("MEMBER_DELAY"), OsString::from("1"));
    }
    let script = match order {
        CompletionOrder::ClosureFirst | CompletionOrder::FailedClosure => CLOSURE_FIRST,
        CompletionOrder::LauncherFirst => LAUNCHER_FIRST,
    };
    let mut command = ProcessCommand::new("/bin/sh");
    command
        .argument("-c")
        .argument(format!("{}{script}", bounded_shell::PREFIX))
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
    *member = Some(GroupGuard::released(
        group,
        vec![paths.go(), paths.close(), paths.finish()],
        EVENT_TIMEOUT,
        POLL_INTERVAL,
    ));
    let closed = |event: &ProcessBrokerEvent| matches!(event, ProcessBrokerEvent::LifetimeClosed { generation } if *generation == service);
    let launcher_exited = |event: &ProcessBrokerEvent| {
        matches!(
            event,
            ProcessBrokerEvent::Child {
                generation,
                event: ChildEvent::Exited { code: 0, .. },
            } if *generation == service
        )
    };
    match order {
        CompletionOrder::ClosureFirst | CompletionOrder::FailedClosure => {
            expect(client, "lifetime closure", closed).await?;
            fs::write(paths.go(), "go")?;
            expect(client, "launcher exit", launcher_exited).await?;
        }
        CompletionOrder::LauncherFirst => {
            expect(client, "launcher exit", launcher_exited).await?;
            fs::write(paths.close(), "close")?;
            expect(client, "lifetime closure", closed).await?;
        }
    }
    Ok(group)
}

async fn expect_ready(client: &mut ProcessBrokerClient) -> Result<(), Box<dyn Error>> {
    expect(client, "broker readiness", |event| {
        matches!(event, ProcessBrokerEvent::Ready)
    })
    .await
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

/// Handshake and marker files shared with the service script.
///
/// Release ownership exists before starting the broker. After signal-capable
/// owners drop, keep all markers through descendant collection and group drain.
struct FixturePaths {
    base: PathBuf,
    cleanup: Option<release_directory::ReleaseDirectory>,
}

impl FixturePaths {
    fn new(name: &str) -> io::Result<Self> {
        let base = env::temp_dir().join(format!(
            "immortal-lifetime-release-{name}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir(&base)?;
        let cleanup = Some(release_directory::ReleaseDirectory::new(
            base.clone(),
            &["go", "close", "finish"],
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

    fn file(&self, name: &str) -> PathBuf {
        self.base.join(name)
    }

    fn ready(&self) -> PathBuf {
        self.file("ready")
    }

    fn mark(&self) -> PathBuf {
        self.file("mark")
    }

    fn go(&self) -> PathBuf {
        self.file("go")
    }

    fn close(&self) -> PathBuf {
        self.file("close")
    }

    fn finish(&self) -> PathBuf {
        self.file("finish")
    }

    /// Fail if the member's `SIGTERM` trap ever ran.
    fn require_unsignalled(&self) -> Result<(), Box<dyn Error>> {
        if exists(&self.mark()) {
            return Err(
                io::Error::other("the broker signalled a completed generation's group").into(),
            );
        }
        Ok(())
    }
}

fn exists(path: &Path) -> bool {
    path.try_exists().unwrap_or(false)
}

impl Drop for FixturePaths {
    fn drop(&mut self) {
        // A daemon may have reported ready before a fallible event read. Its
        // numeric ready file is an absence observation, never signal authority.
        if let Ok(raw) = fs::read_to_string(self.ready())
            && let Ok(pid) = raw.trim().parse::<i32>()
            && let Ok(group) = ProcessGroupId::try_from(pid)
        {
            self.record(group);
        }
        drop(self.cleanup.take());
    }
}
