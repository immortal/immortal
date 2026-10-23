//! Linux helper discovery and bounded shutdown failure/foreground-reap schedules.

mod sibling;

use std::{
    env,
    error::Error,
    ffi::OsStr,
    fs, io,
    path::PathBuf,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand, ProcessId, ProcessSignal,
        SignalTarget, signal, signal_child, start_process_broker,
    },
    supervisor::Generation,
};
use tokio::{
    runtime::Builder,
    time::{sleep, timeout},
};

use crate::broker_guard::BrokerGuard;
use crate::residual_children::assert_no_residual_children;

const CLEANUP: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(5);
const STARTUP: Duration = Duration::from_secs(2);
const HELPERS: usize = 8;

/// Resume stopped helpers before `BrokerGuard`'s fail-closed broker termination.
/// That guard is the sole broker collector, including auxiliary transitions.
struct Scenario {
    broker: ProcessId,
    cleanup: Option<BrokerGuard>,
    helpers: Vec<ProcessId>,
}

impl Scenario {
    fn new(broker: ProcessId) -> Self {
        Self {
            broker,
            cleanup: Some(BrokerGuard::new(broker, CLEANUP, POLL)),
            helpers: Vec::new(),
        }
    }

    fn resume(&mut self) {
        // BrokerGuard is the sole collector, even for auxiliary STOP/CONT.
        let Some(broker) = self.cleanup.as_mut() else {
            return;
        };
        match broker.signal(ProcessSignal::Stop) {
            Ok(Some(_)) => {
                self.resume_adopted();
                return;
            }
            Err(_) => return,
            Ok(None) => {}
        }
        // A stopped broker cannot collect a helper between discovery and CONT.
        let deadline = Instant::now() + STARTUP;
        let stopped = loop {
            match broker.observe() {
                Ok(Some(immortal_core::process::ChildEvent::Stopped { .. })) => break true,
                Ok(Some(event)) if event.is_terminal() => break false,
                Err(_) => break false,
                Ok(_) => {}
            }
            if Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(POLL);
        };
        if stopped && let Ok(owned) = children(self.broker) {
            for helper in &self.helpers {
                if owned.contains(helper) {
                    let _ = signal(SignalTarget::Process(*helper), ProcessSignal::Continue);
                }
            }
        }
        if matches!(broker.signal(ProcessSignal::Continue), Ok(Some(_))) {
            self.resume_adopted();
        }
    }

    /// A collected broker has handed its remaining children to this Linux reaper.
    /// Each exact-child delivery retires any consumed terminal event from the list.
    fn resume_adopted(&mut self) {
        self.helpers
            .retain(|helper| matches!(signal_child(*helper, ProcessSignal::Continue), Ok(None)));
    }
}

impl Drop for Scenario {
    fn drop(&mut self) {
        self.resume();
        drop(self.cleanup.take());
        self.helpers.clear();
        drop(crate::subtree_guard::SubtreeGuard);
    }
}

/// Run budget and sibling-containment schedules. The optional probe mode reuses
/// the same cleanup with an external reproducer and its PID marker.
pub(super) fn run() -> Result<(), Box<dyn Error>> {
    let mut arguments = env::args_os().skip(1);
    let mode = arguments.next();
    if mode.as_deref() == Some(OsStr::new("--broker-exit-probe")) {
        return broker_exit_before_resume_retains_its_outcome();
    }
    if mode.as_deref() == Some(OsStr::new("--sibling-probe")) {
        let program = PathBuf::from(arguments.next().ok_or("missing probe program")?);
        let marker = PathBuf::from(arguments.next().ok_or("missing probe PID marker")?);
        if arguments.next().is_some() {
            return Err("unexpected probe argument".into());
        }
        return sibling::probe(&program, &marker);
    }
    broker_exit_before_resume_retains_its_outcome()?;
    many_stopped_helpers_share_shutdown_budget(false)?;
    many_stopped_helpers_share_shutdown_budget(true)?;
    sibling::run()
}

/// Force the actual auxiliary-resume path to encounter an already-exited broker.
/// The one collector must retain that terminal event for the later explicit wait.
fn broker_exit_before_resume_retains_its_outcome() -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut scenario = Scenario::new(endpoint.process());
    let runtime = Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        if !matches!(receive(&mut client).await?, ProcessBrokerEvent::Ready) {
            return Err(io::Error::other("expected broker readiness").into());
        }
        client.shutdown().await?;
        if !matches!(
            receive(&mut client).await?,
            ProcessBrokerEvent::ShutdownComplete
        ) {
            return Err(io::Error::other("expected shutdown completion").into());
        }
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(runtime);
    let deadline = Instant::now() + CLEANUP;
    loop {
        let status = fs::read_to_string(format!("/proc/{}/status", scenario.broker))?;
        if status
            .lines()
            .any(|line| line.starts_with("State:") && line.contains('Z'))
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("broker did not exit".into());
        }
        std::thread::sleep(POLL);
    }
    scenario.resume();
    scenario
        .cleanup
        .as_mut()
        .ok_or("broker owner missing")?
        .wait()?;
    drop(scenario);
    assert_no_residual_children("broker exit before auxiliary resume")?;
    Ok(())
}

/// Eight unavailable helpers must not add eight separate waits to either
/// phase. Reaps during the sweep must use the same already-expired budget.
fn many_stopped_helpers_share_shutdown_budget(
    exit_during_grace: bool,
) -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut scenario = Scenario::new(endpoint.process());
    let runtime = Builder::new_current_thread().enable_all().build()?;
    let result = runtime.block_on(async {
        let mut client = endpoint.connect()?;
        if !matches!(receive(&mut client).await?, ProcessBrokerEvent::Ready) {
            return Err(io::Error::other("expected broker readiness").into());
        }
        let workloads = start_workloads(&mut client).await?;
        scenario.helpers = children(scenario.broker)?
            .into_iter()
            .filter(|pid| !workloads.contains(pid))
            .collect();
        if scenario.helpers.len() != HELPERS {
            return Err(io::Error::other("unexpected broker helper count").into());
        }
        for helper in &scenario.helpers {
            signal(SignalTarget::Process(*helper), ProcessSignal::Stop)?;
        }
        wait_stopped(&scenario.helpers).await?;
        // The broker suppresses stop events. Allow its 250 ms safety drain to
        // consume them, reproducing the serialized all-child-drain schedule.
        sleep(Duration::from_millis(500)).await;
        let started = Instant::now();
        client.shutdown().await?;
        if exit_during_grace {
            // Helpers remain stopped throughout the two-second TERM sweep.
            // These known /bin/sleep children cannot exit/release their PIDs
            // before our one process-targeted KILL (their self-timeout is 60 s).
            sleep(Duration::from_millis(100)).await;
            for process in workloads {
                signal(SignalTarget::Process(process), ProcessSignal::Kill)?;
            }
        }
        let outcome = timeout(CLEANUP, shutdown_answer(&mut client)).await??;
        let elapsed = started.elapsed();
        scenario.resume();
        if exit_during_grace {
            if !matches!(outcome, ProcessBrokerEvent::ShutdownComplete)
                || elapsed >= Duration::from_secs(4)
            {
                return Err(io::Error::other(format!(
                    "foreground reaps renewed phase budget: {outcome:?}, {elapsed:?}"
                ))
                .into());
            }
            if let Some(broker) = &mut scenario.cleanup {
                broker.wait()?;
            }
            scenario.helpers.clear();
        } else if !matches!(outcome, ProcessBrokerEvent::ShutdownFailed { .. })
            || elapsed >= Duration::from_secs(6)
        {
            return Err(io::Error::other(format!(
                "stopped helpers exceeded shutdown budget: {outcome:?}, {elapsed:?}"
            ))
            .into());
        }
        Ok::<(), Box<dyn Error>>(())
    });
    drop(runtime);
    drop(scenario);
    assert_no_residual_children("stopped-helper signal budget")?;
    result
}

/// Receive only the final ordered-shutdown answer, draining intermediate reaps.
async fn shutdown_answer(
    client: &mut ProcessBrokerClient,
) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    loop {
        match client.next_event().await? {
            event @ (ProcessBrokerEvent::ShutdownFailed { .. }
            | ProcessBrokerEvent::ShutdownComplete) => return Ok(event),
            ProcessBrokerEvent::Child { .. } => {}
            other => {
                return Err(
                    io::Error::other(format!("unexpected shutdown event: {other:?}")).into(),
                );
            }
        }
    }
}

/// Start children which stay alive throughout both shutdown phases unless killed.
async fn start_workloads(
    client: &mut ProcessBrokerClient,
) -> Result<Vec<ProcessId>, Box<dyn Error>> {
    let mut workloads = Vec::new();
    for id in 1..=HELPERS {
        let generation = Generation::new(u64::try_from(id)?).ok_or("invalid generation")?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("60");
        client.spawn(generation, command, STARTUP).await?;
        match receive(client).await? {
            ProcessBrokerEvent::Started { process, .. } => workloads.push(process),
            other => {
                return Err(io::Error::other(format!("unexpected spawn event: {other:?}")).into());
            }
        }
    }
    Ok(workloads)
}

/// Only this unreaped broker's direct children may be selected as helpers.
fn children(process: ProcessId) -> io::Result<Vec<ProcessId>> {
    fs::read_to_string(format!("/proc/{process}/task/{process}/children"))?
        .split_whitespace()
        .map(|value| {
            let pid = value.parse::<i32>().map_err(io::Error::other)?;
            ProcessId::try_from(pid).map_err(io::Error::other)
        })
        .collect()
}

/// Wait for the kernel stop before asking the broker to shut down.
async fn wait_stopped(helpers: &[ProcessId]) -> io::Result<()> {
    let deadline = Instant::now() + STARTUP;
    loop {
        let mut stopped = true;
        for helper in helpers {
            let status = fs::read_to_string(format!("/proc/{helper}/status"))?;
            stopped &= status
                .lines()
                .any(|line| line.starts_with("State:") && line.contains('T'));
        }
        if stopped {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "helpers did not stop",
            ));
        }
        sleep(POLL).await;
    }
}

/// Bound startup observations separately from the shutdown-wide deadline.
async fn receive(client: &mut ProcessBrokerClient) -> Result<ProcessBrokerEvent, Box<dyn Error>> {
    Ok(timeout(CLEANUP, client.next_event()).await??)
}
