//! Fault contract for fail-closed service containment after broker death.
//!
//! Each scenario owns one fresh broker and workload group. The broker is
//! killed rather than disconnected cleanly; its out-of-group helper must then
//! remove the running or stopped group within a hard deadline. Cleanup guards
//! kill any surviving owned group and reap the direct broker on every path.

use std::{
    error::Error,
    io, thread,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        BrokerSignalScope, ChildEvent, ProcessBrokerEndpoint, ProcessBrokerEvent, ProcessCommand,
        ProcessGroupId, ProcessId, ProcessSignal, SignalTarget, reap_any_event, signal,
        start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);

fn main() -> Result<(), Box<dyn Error>> {
    broker_death_contains_group(false)?;
    broker_death_contains_group(true)?;
    broker_death_without_workload_is_bounded()?;
    Ok(())
}

fn broker_death_contains_group(stop_first: bool) -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let broker_process = endpoint.process();
    let mut broker = ForcedBrokerGuard::new(broker_process);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let group = runtime.block_on(start_and_kill_broker(endpoint, stop_first))?;
    drop(runtime);
    let mut group = GroupGuard::new(group);

    require_broker_killed(broker.wait()?, broker_process)?;
    wait_for_group_absence(group.group())?;
    group.mark_absent();
    drain_subtree()?;
    Ok(())
}

async fn start_and_kill_broker(
    endpoint: ProcessBrokerEndpoint,
    stop_first: bool,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    let broker = endpoint.process();
    let mut client = endpoint.connect()?;
    require_ready(&next_event(&mut client).await?)?;

    let generation = Generation::FIRST;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("30");
    client.spawn(generation, command, STARTUP_TIMEOUT).await?;
    let group = require_started(next_event(&mut client).await?, generation)?;
    if stop_first {
        client
            .signal(generation, BrokerSignalScope::Group, ProcessSignal::Stop)
            .await?;
        require_signal_delivered(next_event(&mut client).await?, generation)?;
        require_stopped(next_event(&mut client).await?, generation)?;
    }

    signal(SignalTarget::Process(broker), ProcessSignal::Kill)?;
    match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await {
        Ok(Err(_)) => Ok(group),
        Ok(Ok(event)) => Err(io::Error::other(format!(
            "broker death produced a live protocol event: {event:?}"
        ))
        .into()),
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "broker client did not observe forced broker death",
        )
        .into()),
    }
}

fn broker_death_without_workload_is_bounded() -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let broker_process = endpoint.process();
    let mut broker = ForcedBrokerGuard::new(broker_process);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        require_ready(&next_event(&mut client).await?)?;
        signal(SignalTarget::Process(broker_process), ProcessSignal::Kill)?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await {
            Ok(Err(_)) => Ok::<(), Box<dyn Error>>(()),
            Ok(Ok(event)) => Err(io::Error::other(format!(
                "empty broker death produced a live protocol event: {event:?}"
            ))
            .into()),
            Err(_) => Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "empty broker death did not close its client",
            )
            .into()),
        }
    })?;
    drop(runtime);
    require_broker_killed(broker.wait()?, broker_process)?;
    drain_subtree()?;
    Ok(())
}

async fn next_event(
    client: &mut immortal_core::process::ProcessBrokerClient,
) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??)
}

fn require_ready(event: &ProcessBrokerEvent) -> Result<(), Box<dyn Error>> {
    if *event == ProcessBrokerEvent::Ready {
        Ok(())
    } else {
        Err(io::Error::other(format!("expected broker readiness, received {event:?}")).into())
    }
}

fn require_started(
    event: ProcessBrokerEvent,
    expected: Generation,
) -> Result<ProcessGroupId, Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Started {
            generation, group, ..
        } if generation == expected => Ok(group),
        event => {
            Err(io::Error::other(format!("expected generation start, received {event:?}")).into())
        }
    }
}

fn require_signal_delivered(
    event: ProcessBrokerEvent,
    expected: Generation,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::SignalDelivered { generation } if generation == expected => Ok(()),
        event => Err(io::Error::other(format!(
            "expected stop acknowledgement, received {event:?}"
        ))
        .into()),
    }
}

fn require_stopped(event: ProcessBrokerEvent, expected: Generation) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Stopped { .. },
        } if generation == expected => Ok(()),
        event => {
            Err(io::Error::other(format!("expected stopped generation, received {event:?}")).into())
        }
    }
}

fn require_broker_killed(
    event: ChildEvent,
    expected: immortal_core::process::ProcessId,
) -> Result<(), Box<dyn Error>> {
    match event {
        ChildEvent::Signaled {
            pid,
            signal: observed,
        } if pid == expected && observed == u8::try_from(libc::SIGKILL)? => Ok(()),
        event => Err(io::Error::other(format!(
            "expected forced broker SIGKILL, received {event:?}"
        ))
        .into()),
    }
}

fn wait_for_group_absence(group: ProcessGroupId) -> io::Result<()> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        reap_orphans()?;
        match signal(SignalTarget::Group(group), ProcessSignal::WindowChange) {
            Err(error) if error.raw_os_error() == Some(libc::ESRCH) => return Ok(()),
            Ok(()) => {}
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "workload group survived forced broker death",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

/// Reap any workload-subtree zombie reparented to this supervising process.
///
/// The forced broker held the child-subreaper role, so its killed workloads
/// reparent to this process — the broker's own reaper — rather than init. On
/// FreeBSD init never reaps a process orphaned from an exited reaper, so the
/// supervisor must drain those zombies itself for the owned group to empty.
/// `ECHILD` (no children remain) is benign here.
fn reap_orphans() -> io::Result<()> {
    match reap_any_event() {
        Ok(Some(_) | None) => Ok(()),
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Drain the remaining reparented subtree until no child of this process exists.
///
/// After the owned group is gone the broker's out-of-group group guard is still
/// a zombie reparented to this process; leaving it unreaped would leak it to
/// init, which on FreeBSD never collects a reaper's orphan. Draining to
/// `ECHILD` proves the whole supervised subtree was reaped, matching the
/// executor's post-broker-death reap.
fn drain_subtree() -> io::Result<()> {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        match reap_any_event() {
            Ok(Some(_) | None) => {}
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Ok(()),
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "supervised subtree not fully reaped after broker death",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

struct GroupGuard {
    absent: bool,
    group: ProcessGroupId,
}

impl GroupGuard {
    const fn new(group: ProcessGroupId) -> Self {
        Self {
            absent: false,
            group,
        }
    }

    const fn group(&self) -> ProcessGroupId {
        self.group
    }

    const fn mark_absent(&mut self) {
        self.absent = true;
    }
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if !self.absent {
            let _ = signal(SignalTarget::Group(self.group), ProcessSignal::Kill);
        }
    }
}

struct ForcedBrokerGuard {
    process: ProcessId,
    reaped: bool,
}

impl ForcedBrokerGuard {
    const fn new(process: ProcessId) -> Self {
        Self {
            process,
            reaped: false,
        }
    }

    fn wait(&mut self) -> io::Result<ChildEvent> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match reap_any_event()? {
                Some(event) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return Ok(event);
                }
                // An orphaned workload-subtree process reparented here after the
                // broker died, or an idle poll; keep waiting for the broker's
                // own terminal event.
                Some(_) | None => {}
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for forced broker death",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for ForcedBrokerGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = signal(SignalTarget::Process(self.process), ProcessSignal::Kill);
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while Instant::now() < deadline {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return;
                }
                Ok(Some(_) | None) => thread::sleep(POLL_INTERVAL),
                Err(_) => return,
            }
        }
    }
}
