//! Fresh-process contract for descriptor cleanup after supervisor IPC loss.
//!
//! After the supervisor connection is lost the broker runs the configured stop
//! command, waits for the tracked lifetime descriptor to close, and must reap
//! every member of the groups it killed or released before it exits, so
//! nothing reparents to this process. The residual check after each broker
//! exit proves that.

#[path = "support/bounded_shell.rs"]
mod bounded_shell;
#[path = "support/broker_guard.rs"]
mod broker_guard;
#[path = "support/release_directory.rs"]
mod release_directory;
#[path = "support/residual_children.rs"]
mod residual_children;
#[path = "support/subtree_guard.rs"]
mod subtree_guard;

use std::{
    error::Error,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        BrokerLifetimePlan, ChildEvent, ProcessBrokerEvent, ProcessCommand, ProcessEnvironment,
        ProcessGroupId, ProcessId, ProcessSignal, reap_event, signal_child,
        start_process_broker_with_lifetime,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;
use crate::residual_children::assert_no_residual_children;

/// The broker exit status when supervisor-loss cleanup cannot complete.
const BROKER_EXIT_SOFTWARE: u8 = 70;
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const HOOK_STOP_TIMEOUT: Duration = Duration::from_millis(300);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> Result<(), Box<dyn Error>> {
    let _orphans = subtree_guard::SubtreeGuard;
    reject_zero_cleanup_deadline()?;
    lifetime_closure_drains_the_tracked_group()?;
    overrunning_stop_hook_group_is_drained()?;
    Ok(())
}

/// The stop hook ends the tracked service, and its leftovers drain before exit.
///
/// One member of the tracked group closed the lifetime descriptor and outlives
/// the descriptor's closure. The broker must wait for that member to exit and
/// reap it rather than exit at closure and hand it to this process.
fn lifetime_closure_drains_the_tracked_group() -> Result<(), Box<dyn Error>> {
    let paths = LossPaths::new("descriptor")?;
    let endpoint = start_process_broker_with_lifetime(cleanup_plan(&paths)?)?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let mut descendant = DescendantGuard::new(paths.done(), paths.stop());
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_until_supervisor_loss(endpoint, &paths))?;
    drop(runtime);

    broker.wait()?;
    assert_no_residual_children("lifetime closure cleanup")?;
    wait_for_path(paths.done(), EVENT_TIMEOUT)?;
    descendant.mark_complete();
    Ok(())
}

/// An overrunning stop hook's whole group is killed and drained before exit.
///
/// The hook signals the service, then backgrounds a member and outlives its
/// stop timeout. The broker kills the hook's group, must reap every member of
/// it, still lets the tracked lifetime close, and then exits with its cleanup
/// failure status because the hook never finished on its own.
fn overrunning_stop_hook_group_is_drained() -> Result<(), Box<dyn Error>> {
    let paths = LossPaths::new("hook-overrun")?;
    let endpoint = start_process_broker_with_lifetime(overrunning_hook_plan(&paths)?)?;
    let mut broker = FailingBroker::new(endpoint.process());
    let mut descendant = DescendantGuard::new(paths.done(), paths.stop());
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_until_supervisor_loss(endpoint, &paths))?;
    drop(runtime);

    broker.wait_for_exit(BROKER_EXIT_SOFTWARE)?;
    assert_no_residual_children("overrunning stop hook cleanup")?;
    wait_for_path(paths.done(), EVENT_TIMEOUT)?;
    descendant.mark_complete();
    Ok(())
}

fn reject_zero_cleanup_deadline() -> Result<(), Box<dyn Error>> {
    if BrokerLifetimePlan::new(
        ProcessCommand::new("/usr/bin/true"),
        Duration::ZERO,
        EVENT_TIMEOUT,
    )
    .is_ok()
    {
        return Err(io::Error::other("zero broker cleanup timeout was accepted").into());
    }
    Ok(())
}

fn cleanup_plan(paths: &LossPaths) -> io::Result<BrokerLifetimePlan> {
    let mut environment = ProcessEnvironment::new();
    environment.insert(
        OsString::from("STOP"),
        paths.stop().as_os_str().to_os_string(),
    );
    let mut stop = ProcessCommand::new("/bin/sh");
    stop.argument("-c")
        .argument(": > \"$STOP\"")
        .environment(environment);
    BrokerLifetimePlan::new(stop, STARTUP_TIMEOUT, EVENT_TIMEOUT)
}

fn overrunning_hook_plan(paths: &LossPaths) -> io::Result<BrokerLifetimePlan> {
    let mut environment = ProcessEnvironment::new();
    environment.insert(
        OsString::from("STOP"),
        paths.stop().as_os_str().to_os_string(),
    );
    let mut stop = ProcessCommand::new("/bin/sh");
    stop.argument("-c")
        .argument(": > \"$STOP\"; /bin/sleep 30 & exec /bin/sleep 30")
        .environment(environment);
    BrokerLifetimePlan::new(stop, HOOK_STOP_TIMEOUT, EVENT_TIMEOUT)
}

async fn run_until_supervisor_loss(
    endpoint: immortal_core::process::ProcessBrokerEndpoint,
    paths: &LossPaths,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    let mut client = endpoint.connect()?;
    if client.next_event().await? != ProcessBrokerEvent::Ready {
        return Err(io::Error::other("broker did not become ready").into());
    }
    let generation = Generation::FIRST;
    let mut environment = ProcessEnvironment::new();
    environment.insert(
        OsString::from("STOP"),
        paths.stop().as_os_str().to_os_string(),
    );
    environment.insert(
        OsString::from("DONE"),
        paths.done().as_os_str().to_os_string(),
    );
    let mut service = ProcessCommand::new("/bin/sh");
    service
        .argument("-c")
        .argument(format!(
            "{}sleep 1 4>&- & (wait_for \"$STOP\"; printf stopped > \"$DONE\") & exit 0",
            bounded_shell::PREFIX
        ))
        .environment(environment);
    client
        .spawn_with_lifetime(generation, service, STARTUP_TIMEOUT, None)
        .await?;
    let group = match client.next_event().await? {
        ProcessBrokerEvent::Started {
            generation: current,
            group,
            ..
        } if current == generation => group,
        event => {
            return Err(io::Error::other(format!(
                "expected descriptor generation start, received {event:?}"
            ))
            .into());
        }
    };
    paths.cleanup.record(group);
    match client.next_event().await? {
        ProcessBrokerEvent::Child {
            generation: current,
            event: ChildEvent::Exited { code: 0, .. },
        } if current == generation => {}
        event => {
            return Err(
                io::Error::other(format!("expected launcher exit, received {event:?}")).into(),
            );
        }
    }
    drop(client);
    Ok(group)
}

fn wait_for_path(path: &Path, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(io::Error::new(
        io::ErrorKind::TimedOut,
        "supervisor-loss stop hook did not terminate the descriptor service",
    ))
}

struct LossPaths {
    done: PathBuf,
    stop: PathBuf,
    cleanup: release_directory::ReleaseDirectory,
}

impl LossPaths {
    fn new(name: &str) -> io::Result<Self> {
        let base = std::env::temp_dir().join(format!(
            "immortal-broker-supervisor-loss-{name}-{}",
            std::process::id()
        ));
        fs::create_dir(&base)?;
        let done = base.join("done");
        let stop = base.join("stop");
        let cleanup = release_directory::ReleaseDirectory::new(base, &["stop"]);
        Ok(Self {
            done,
            stop,
            cleanup,
        })
    }

    fn done(&self) -> &Path {
        &self.done
    }

    fn stop(&self) -> &Path {
        &self.stop
    }
}

struct DescendantGuard<'path> {
    complete: bool,
    done: &'path Path,
    stop: &'path Path,
}

impl<'path> DescendantGuard<'path> {
    const fn new(done: &'path Path, stop: &'path Path) -> Self {
        Self {
            complete: false,
            done,
            stop,
        }
    }

    const fn mark_complete(&mut self) {
        self.complete = true;
    }
}

impl Drop for DescendantGuard<'_> {
    fn drop(&mut self) {
        if self.complete || self.done.exists() {
            return;
        }
        let _ = fs::write(self.stop, "release");
        let _ = wait_for_path(self.done, EVENT_TIMEOUT);
    }
}

/// Reaps a broker expected to exit with a cleanup failure status.
///
/// Dropping it before the broker was reaped kills and reaps the broker, so a
/// failing scenario leaves nothing behind; once reaped, its PID is never
/// signalled again.
struct FailingBroker {
    process: ProcessId,
    reaped: bool,
}

impl FailingBroker {
    const fn new(process: ProcessId) -> Self {
        Self {
            process,
            reaped: false,
        }
    }

    fn wait_for_exit(&mut self, expected: u8) -> io::Result<()> {
        let deadline = Instant::now() + EVENT_TIMEOUT + EVENT_TIMEOUT;
        loop {
            match reap_event(self.process)? {
                Some(event) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return match event {
                        ChildEvent::Exited { code, .. } if code == expected => Ok(()),
                        event => Err(io::Error::other(format!(
                            "broker exited with {event:?}, expected status {expected}"
                        ))),
                    };
                }
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
}

impl Drop for FailingBroker {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        match signal_child(self.process, ProcessSignal::Kill) {
            Ok(Some(_)) => self.reaped = true,
            Ok(None) => {
                let _ = self.wait_for_exit(BROKER_EXIT_SOFTWARE);
            }
            Err(_) => {}
        }
    }
}
