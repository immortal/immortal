//! Foreground-reap containment with a second member that ignores TERM.
//!
//! The fixture keeps the sibling in its leader's group and reports its PID only
//! after installing `SIG_IGN`. Cleanup resumes helpers, terminates the unreaped
//! broker, then kills any surviving sibling only after it is our adopted child.
//! The survivor observation is recorded before fallback cleanup can hide it.

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
        ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand, ProcessEnvironment,
        ProcessGroupId, ProcessId, ProcessSignal, SignalTarget, signal, signal_child,
        start_process_broker,
    },
    supervisor::Generation,
};
use tokio::{
    runtime::Builder,
    time::{sleep, timeout},
};

use super::{
    CLEANUP, HELPERS, POLL, STARTUP, Scenario, children, receive, shutdown_answer, wait_stopped,
};
use crate::residual_children::assert_no_residual_children;

/// Own the marker directory, removing it on every return path.
struct Files {
    directory: PathBuf,
    marker: PathBuf,
}

impl Files {
    fn new(stopped: bool, marker: Option<&Path>) -> io::Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "immortal-signal-sibling-{}-{stopped}",
            std::process::id()
        ));
        fs::create_dir(&directory)?;
        let marker = marker.map_or_else(|| directory.join("sibling.pid"), Path::to_path_buf);
        let files = Self { directory, marker };
        // A probe marker from a previous run must not select an unrelated PID.
        if files.marker.exists() {
            fs::remove_file(&files.marker)?;
        }
        Ok(files)
    }
}

impl Drop for Files {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.marker);
        let _ = fs::remove_dir(&self.directory);
    }
}

/// `BrokerGuard` retains every terminal outcome through the survivor observation.
struct Fixture {
    scenario: Scenario,
    files: Files,
    sibling: Option<ProcessId>,
}

impl Fixture {
    /// Observe after broker exit, before fixture cleanup kills any survivor.
    fn survivor_after_broker_exit(&mut self) -> io::Result<bool> {
        self.scenario.resume();
        let broker = self
            .scenario
            .cleanup
            .as_mut()
            .ok_or_else(|| io::Error::other("broker owner missing"))?;
        broker.signal(ProcessSignal::Kill)?;
        let deadline = Instant::now() + CLEANUP;
        loop {
            if broker
                .observe()?
                .is_some_and(immortal_core::process::ChildEvent::is_terminal)
            {
                return self.sibling.map_or(Ok(false), running);
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "broker did not exit",
                ));
            }
            thread::sleep(POLL);
        }
    }

    /// Kill only an unreaped child adopted here; procfs membership plus exclusive
    /// reaping keeps its positive PID pinned until the following cleanup drain.
    fn kill_adopted_survivor(&self) {
        let Some(sibling) = self.sibling else {
            return;
        };
        let Ok(owner) = i32::try_from(std::process::id())
            .map_err(io::Error::other)
            .and_then(|pid| ProcessId::try_from(pid).map_err(io::Error::other))
        else {
            return;
        };
        let deadline = Instant::now() + CLEANUP;
        while Instant::now() < deadline {
            if children(owner).is_ok_and(|owned| owned.contains(&sibling)) {
                let _ = signal_child(sibling, ProcessSignal::Kill);
                return;
            }
            if !matches!(running(sibling), Ok(true)) {
                return;
            }
            thread::sleep(POLL);
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.scenario.resume();
        if let Some(broker) = &mut self.scenario.cleanup {
            let _ = broker.signal(ProcessSignal::Kill);
        }
        self.kill_adopted_survivor();
        drop(self.scenario.cleanup.take());
        // The broker was just reaped; do not retain helper IDs for another resume.
        self.scenario.helpers.clear();
    }
}

/// Compare healthy delivery with the expired-budget/stopped-helper schedule.
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    foreground_sibling_is_contained(false, None)?;
    foreground_sibling_is_contained(true, None)
}

/// Run the same schedule with the supplied original reproducer and PID marker.
pub(super) fn probe(program: &Path, marker: &Path) -> Result<(), Box<dyn Error>> {
    foreground_sibling_is_contained(false, Some((program, marker)))?;
    foreground_sibling_is_contained(true, Some((program, marker)))
}

/// A skipped post-reap kill must close the guard fail-closed, not disarm it.
fn foreground_sibling_is_contained(
    stopped: bool,
    external: Option<(&Path, &Path)>,
) -> Result<(), Box<dyn Error>> {
    let files = Files::new(stopped, external.map(|(_, marker)| marker))?;
    let endpoint = start_process_broker()?;
    let mut fixture = Fixture {
        scenario: Scenario::new(endpoint.process()),
        files,
        sibling: None,
    };
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        if !matches!(receive(&mut client).await?, ProcessBrokerEvent::Ready) {
            return Err(io::Error::other("expected broker readiness").into());
        }
        let (workloads, group) = start_sibling_workloads(
            &mut client,
            &fixture.files.marker,
            external.map(|(program, _)| program),
        )
        .await?;
        fixture.sibling = Some(read_sibling(&fixture.files.marker, group).await?);
        fixture.scenario.helpers = children(fixture.scenario.broker)?
            .into_iter()
            .filter(|pid| !workloads.contains(pid))
            .collect();
        if fixture.scenario.helpers.len() != HELPERS {
            return Err(io::Error::other("unexpected helper count for sibling scenario").into());
        }
        if stopped {
            for helper in &fixture.scenario.helpers {
                signal(SignalTarget::Process(*helper), ProcessSignal::Stop)?;
            }
            wait_stopped(&fixture.scenario.helpers).await?;
            // Allow the broker's 250 ms safety sweep to consume STOP events.
            sleep(Duration::from_millis(500)).await;
        }
        let started = Instant::now();
        client.shutdown().await?;
        if stopped {
            // Exit leaders while the TERM sweep blocks on stopped helpers. The
            // sibling ignores the queued TERM and needs EOF's fail-closed KILL.
            sleep(Duration::from_millis(100)).await;
            for process in workloads {
                signal(SignalTarget::Process(process), ProcessSignal::Kill)?;
            }
        }
        let outcome = timeout(CLEANUP, shutdown_answer(&mut client)).await??;
        Ok::<_, Box<dyn Error>>((outcome, started.elapsed()))
    });
    let survivor = fixture.survivor_after_broker_exit();
    drop(runtime);
    drop(fixture);
    assert_no_residual_children("foreground sibling containment")?;
    let (outcome, elapsed) = result?;
    let survivor = survivor?;
    eprintln!(
        "sibling scenario stopped={stopped}: {outcome:?}, elapsed={elapsed:?}, sibling_alive_after_broker_exit={survivor}"
    );
    if survivor {
        return Err(
            io::Error::other("TERM-ignoring sibling survived shutdown and broker exit").into(),
        );
    }
    if !matches!(outcome, ProcessBrokerEvent::ShutdownComplete) {
        return Err(io::Error::other(format!("sibling group did not drain: {outcome:?}")).into());
    }
    Ok(())
}

/// Place one TERM-ignoring sibling in a foreground group, alongside seven plain
/// foreground groups. The shell fixture has a 60 s self-timeout as a last resort.
async fn start_sibling_workloads(
    client: &mut ProcessBrokerClient,
    marker: &Path,
    external: Option<&Path>,
) -> Result<(Vec<ProcessId>, ProcessGroupId), Box<dyn Error>> {
    let mut workloads = Vec::new();
    let mut sibling_group = None;
    for id in 1..=HELPERS {
        let generation = Generation::new(u64::try_from(id)?).ok_or("invalid generation")?;
        let command = if id == 1 {
            sibling_command(marker, external)
        } else {
            let mut command = ProcessCommand::new("/bin/sleep");
            command.argument("60");
            command
        };
        // Materialization is owned by the client, not borrowed by the broker.
        client.spawn(generation, command, STARTUP).await?;
        match receive(client).await? {
            ProcessBrokerEvent::Started { process, group, .. } => {
                workloads.push(process);
                if id == 1 {
                    sibling_group = Some(group);
                }
            }
            other => {
                return Err(
                    io::Error::other(format!("unexpected sibling start: {other:?}")).into(),
                );
            }
        }
    }
    Ok((
        workloads,
        sibling_group.ok_or("sibling generation was not started")?,
    ))
}

/// Exec preserves the sibling's ignored TERM; no session/group escape occurs.
fn sibling_command(marker: &Path, external: Option<&Path>) -> ProcessCommand {
    if let Some(program) = external {
        return ProcessCommand::new(program);
    }
    let mut environment = ProcessEnvironment::new();
    environment.insert(
        OsString::from("SIBLING_PID"),
        marker.as_os_str().to_os_string(),
    );
    let mut command = ProcessCommand::new("/bin/sh");
    command.argument("-c").argument(concat!(
        "/bin/sh -c 'trap \"\" TERM; printf \"%s\\n\" \"$$\" > \"$SIBLING_PID\"; exec /bin/sleep 60' & ",
        "exec /bin/sleep 60"
    )).environment(environment);
    command
}

/// Observe the readiness marker, verifying membership before remembering a PID.
async fn read_sibling(marker: &Path, group: ProcessGroupId) -> Result<ProcessId, Box<dyn Error>> {
    let deadline = Instant::now() + STARTUP;
    loop {
        match fs::read_to_string(marker) {
            Ok(text) if !text.trim().is_empty() => {
                let process = ProcessId::try_from(text.trim().parse::<i32>()?)?;
                if proc_group(process)? != group {
                    return Err(io::Error::other("sibling escaped the guarded group").into());
                }
                return Ok(process);
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "sibling was not ready").into());
        }
        sleep(POLL).await;
    }
}

/// Verify fixture group membership using Linux procfs without adding a public
/// process API solely for the test. Fields after comm are state, ppid, pgrp.
fn proc_group(process: ProcessId) -> io::Result<ProcessGroupId> {
    let stat = fs::read_to_string(format!("/proc/{process}/stat"))?;
    let (_, fields) = stat
        .rsplit_once(')')
        .ok_or_else(|| io::Error::other("missing proc comm"))?;
    let group = fields
        .split_whitespace()
        .nth(2)
        .ok_or_else(|| io::Error::other("missing proc group"))?;
    ProcessGroupId::try_from(group.parse::<i32>().map_err(io::Error::other)?)
        .map_err(io::Error::other)
}

/// A zombie is no live survivor; the following cleanup drain still reaps it.
fn running(process: ProcessId) -> io::Result<bool> {
    match fs::read_to_string(format!("/proc/{process}/status")) {
        Ok(status) => Ok(!status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}
