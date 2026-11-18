//! A stalled process broker, proven through the real supervisor binary.
//!
//! Stopping the broker with `SIGSTOP` stands in for a starved host or a paused
//! virtual machine: the broker is alive but answers nothing. The supervisor
//! must wait for it, report the wait, and finish the operation once the broker
//! runs again, never end supervision and the service with it.

use std::{error::Error, fs, path::Path, process::Command, thread, time::Duration};

use immortal_core::{
    control::Operation,
    exit::ExitClass,
    process::{ProcessId, ProcessSignal, SignalTarget, signal as deliver_signal},
    status::ServiceState,
};

use crate::COMMAND_TIMEOUT;
use crate::control_client::{
    assert_status, lifecycle_request, require_ok, status, wait_for_new_ready, wait_for_state,
};
use crate::fixtures::{ChildGuard, ConfigFile, RuntimeDirectory, spawn_immortal};

/// How long the broker stays stopped: longer than the ten seconds after which
/// the supervisor used to give up on a restart, five of stop grace and five
/// for the kill's reap.
const BROKER_STALL: Duration = Duration::from_secs(12);

/// What the supervisor writes while it waits for a late broker answer.
const OVERDUE_REPORT: &str = "the process broker has not confirmed";

/// A broker stopped for the test, resumed on every path out of it.
struct StoppedBroker(ProcessId);

impl StoppedBroker {
    fn stop(broker: ProcessId) -> Result<Self, Box<dyn Error>> {
        deliver_signal(SignalTarget::Process(broker), ProcessSignal::Stop)?;
        Ok(Self(broker))
    }
}

impl Drop for StoppedBroker {
    fn drop(&mut self) {
        let _ = deliver_signal(SignalTarget::Process(self.0), ProcessSignal::Continue);
    }
}

/// The supervisor's only child: its process broker.
///
/// Each column gets its own `-o`: BSD `ps` reads `pid=,ppid=` as one column
/// whose header is `,ppid=`.
fn broker_of(supervisor: u32) -> Result<ProcessId, Box<dyn Error>> {
    let output = Command::new("ps")
        .args(["-A", "-o", "pid=", "-o", "ppid="])
        .output()?;
    let listing = String::from_utf8(output.stdout)?;
    let children: Vec<i32> = listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse::<i32>().ok()?;
            let parent = fields.next()?.parse::<u32>().ok()?;
            (parent == supervisor).then_some(pid)
        })
        .collect();
    match children.as_slice() {
        [broker] => ProcessId::new(*broker).ok_or_else(|| "invalid broker pid".into()),
        other => Err(format!("expected one broker child, found {other:?}").into()),
    }
}

/// Whether `pid` still names a process.
fn process_exists(pid: u32) -> Result<bool, Box<dyn Error>> {
    let output = Command::new("ps")
        .args(["-o", "pid=", "-p", &pid.to_string()])
        .output()?;
    Ok(!String::from_utf8(output.stdout)?.trim().is_empty())
}

/// Regression: a broker stalled for twelve seconds during a restart ended the
/// supervisor ("process broker timed out during service termination") and
/// its service with it.
pub(crate) fn prove_a_stalled_broker_delays_but_does_not_end_a_restart(
    binary: &Path,
) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("broker-stall")?;
    let config = ConfigFile::new(
        "broker-stall",
        "version: 2\ncommand: [/bin/sleep, \"30\"]\nrestart:\n  policy: always\n",
    )?;
    let mut supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    let generation = wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    let replaced = status(runtime.socket())?
        .status
        .and_then(|snapshot| snapshot.main_pid)
        .ok_or("ready service has no main pid")?;

    let stalled = StoppedBroker::stop(broker_of(supervisor.id())?)?;
    let restart = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Restart,
        generation,
    )?;
    require_ok(&restart, "restart while the broker is stalled")?;
    thread::sleep(BROKER_STALL);
    if let Some(exit) = supervisor.exited()? {
        return Err(format!("the supervisor gave up on a stalled broker: {exit}").into());
    }
    if !runtime.socket().exists() {
        return Err("the supervisor ended supervision while the broker was stalled".into());
    }
    drop(stalled);

    let replacement = wait_for_new_ready(runtime.socket(), generation, COMMAND_TIMEOUT)?;
    if process_exists(replaced)? {
        return Err(format!("the replaced service {replaced} outlived its restart").into());
    }
    let diagnostics = fs::read_to_string(runtime.root().join("immortal.stderr"))?;
    if !diagnostics.contains(OVERDUE_REPORT) {
        return Err(format!("the wait was not reported: {diagnostics:?}").into());
    }

    let halt = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Halt,
        replacement,
    )?;
    require_ok(&halt, "halt after a stalled broker")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "supervisor surviving a stalled broker",
    )
}
