//! Adversarial resource contracts for the process-broker boundary.
//!
//! One single-threaded parent owns the broker, its client, and every deadline.
//! A bounded stop/continue storm proves that request and child-event routing
//! remains live without accumulating per-signal state. Descriptor exhaustion is
//! isolated in a re-executed subprocess whose reduced file limit cannot affect
//! the test runner; broker startup must return the operating-system error
//! without creating an unreaped child.

#[path = "support/broker_guard.rs"]
mod broker_guard;

use std::{
    env,
    error::Error,
    ffi::OsStr,
    fs::File,
    io,
    process::{Child, Command, ExitStatus},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        BrokerSignalScope, ChildEvent, ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand,
        ProcessSignal, reap_any_event, start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;

const CHILD_MODE: &str = "IMMORTAL_DESCRIPTOR_EXHAUSTION_CHILD";
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const STORM_CYCLES: u16 = 128;
const STORM_TIMEOUT: Duration = Duration::from_secs(30);
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn Error>> {
    if env::var_os(CHILD_MODE).is_some() {
        return descriptor_exhaustion_child();
    }
    signal_storm_is_bounded()?;
    descriptor_exhaustion_is_bounded()?;
    Ok(())
}

fn signal_storm_is_bounded() -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(STORM_TIMEOUT, run_signal_storm(endpoint.connect()?))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process broker did not drain the bounded signal storm",
                )
            })??;
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(runtime);
    broker.wait()?;
    Ok(())
}

async fn run_signal_storm(mut client: ProcessBrokerClient) -> Result<(), Box<dyn Error>> {
    require_event(&mut client, |event| {
        matches!(event, ProcessBrokerEvent::Ready)
    })
    .await?;
    let generation = Generation::FIRST;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("30");
    client.spawn(generation, command, STARTUP_TIMEOUT).await?;
    require_event(&mut client, |event| {
        matches!(
            event,
            ProcessBrokerEvent::Started {
                generation: observed,
                ..
            } if *observed == generation
        )
    })
    .await?;

    for _ in 0..STORM_CYCLES {
        deliver_and_observe(&mut client, generation, ProcessSignal::Stop, |event| {
            matches!(event, ChildEvent::Stopped { .. })
        })
        .await?;
        deliver_and_observe(&mut client, generation, ProcessSignal::Continue, |event| {
            matches!(event, ChildEvent::Continued { .. })
        })
        .await?;
    }

    deliver_and_observe(
        &mut client,
        generation,
        ProcessSignal::Kill,
        ChildEvent::is_terminal,
    )
    .await?;
    client.shutdown().await?;
    require_event(&mut client, |event| {
        matches!(event, ProcessBrokerEvent::ShutdownComplete)
    })
    .await
}

async fn deliver_and_observe(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    signal: ProcessSignal,
    child_matches: impl FnOnce(ChildEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    client
        .signal(generation, BrokerSignalScope::Group, signal)
        .await?;
    require_event(client, |event| {
        matches!(
            event,
            ProcessBrokerEvent::SignalDelivered {
                generation: observed
            } if *observed == generation
        )
    })
    .await?;
    require_event(client, |event| {
        matches!(
            event,
            ProcessBrokerEvent::Child {
                generation: observed,
                event,
            } if *observed == generation && child_matches(*event)
        )
    })
    .await
}

async fn require_event(
    client: &mut ProcessBrokerClient,
    predicate: impl FnOnce(&ProcessBrokerEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    let event = client.next_event().await?;
    if predicate(&event) {
        Ok(())
    } else {
        Err(io::Error::other(format!("unexpected process-broker event: {event:?}")).into())
    }
}

fn descriptor_exhaustion_is_bounded() -> Result<(), Box<dyn Error>> {
    let executable = env::current_exe()?;
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg("set -e; ulimit -n 32; exec \"$1\"")
        .arg("immortal-resource-fault")
        .arg(executable)
        .env(CHILD_MODE, OsStr::new("1"))
        .spawn()?;
    let status = wait_for_subprocess(child, SUBPROCESS_TIMEOUT)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "descriptor-exhaustion subprocess failed with {status}"
        ))
        .into())
    }
}

fn wait_for_subprocess(mut child: Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "descriptor-exhaustion subprocess exceeded its deadline",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn descriptor_exhaustion_child() -> Result<(), Box<dyn Error>> {
    let mut descriptors = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(descriptor) => descriptors.push(descriptor),
            Err(error) if is_descriptor_exhaustion(&error) => break,
            Err(error) => return Err(error.into()),
        }
        if descriptors.len() > 64 {
            return Err(io::Error::other(
                "descriptor limit was not reduced for the exhaustion subprocess",
            )
            .into());
        }
    }

    let error = start_process_broker()
        .err()
        .ok_or_else(|| io::Error::other("broker started with no descriptor capacity"))?;
    if !is_descriptor_exhaustion(&error) {
        return Err(io::Error::other(format!(
            "broker returned an unexpected exhaustion error: {error}"
        ))
        .into());
    }
    match reap_any_event() {
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => Ok(()),
        Ok(Some(event)) => Err(io::Error::other(format!(
            "descriptor exhaustion left a waitable broker child: {event:?}"
        ))
        .into()),
        Ok(None) => Err(io::Error::other("descriptor exhaustion left a live broker child").into()),
        Err(error) => Err(error.into()),
    }
}

fn is_descriptor_exhaustion(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}
