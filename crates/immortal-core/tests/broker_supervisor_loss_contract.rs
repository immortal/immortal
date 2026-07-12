//! Fresh-process contract for descriptor cleanup after supervisor IPC loss.

#[path = "support/broker_guard.rs"]
mod broker_guard;

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
        ProcessGroupId, ProcessSignal, SignalTarget, signal, start_process_broker_with_lifetime,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> Result<(), Box<dyn Error>> {
    reject_zero_cleanup_deadline()?;
    let paths = LossPaths::new();
    let endpoint = start_process_broker_with_lifetime(cleanup_plan(&paths)?)?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let group = runtime.block_on(run_until_supervisor_loss(endpoint, &paths))?;
    drop(runtime);

    let mut descendant = DescendantGuard::new(group, paths.done());
    broker.wait()?;
    wait_for_path(paths.done(), EVENT_TIMEOUT)?;
    descendant.mark_complete();
    Ok(())
}

fn reject_zero_cleanup_deadline() -> Result<(), Box<dyn Error>> {
    if BrokerLifetimePlan::new(
        ProcessCommand::new("/bin/true"),
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
        .argument(
            "(while [ ! -e \"$STOP\" ]; do /bin/sleep 0.02; done; printf stopped > \"$DONE\") & exit 0",
        )
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
}

impl LossPaths {
    fn new() -> Self {
        let base = std::env::temp_dir().join(format!(
            "immortal-broker-supervisor-loss-{}",
            std::process::id()
        ));
        let done = base.with_extension("done");
        let stop = base.with_extension("stop");
        let _ = fs::remove_file(&done);
        let _ = fs::remove_file(&stop);
        Self { done, stop }
    }

    fn done(&self) -> &Path {
        &self.done
    }

    fn stop(&self) -> &Path {
        &self.stop
    }
}

impl Drop for LossPaths {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.done);
        let _ = fs::remove_file(&self.stop);
    }
}

struct DescendantGuard<'path> {
    complete: bool,
    done: &'path Path,
    group: ProcessGroupId,
}

impl<'path> DescendantGuard<'path> {
    const fn new(group: ProcessGroupId, done: &'path Path) -> Self {
        Self {
            complete: false,
            done,
            group,
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
        let _ = signal(SignalTarget::Group(self.group), ProcessSignal::Kill);
    }
}
