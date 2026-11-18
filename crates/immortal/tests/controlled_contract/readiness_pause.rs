//! Readiness across job control, proven through the real supervisor binary.
//!
//! The broker's readiness clock keeps running while job control holds the
//! service, so a pause that outlasts the readiness timeout fails readiness.
//! The supervisor must act on that failure when the service continues, never
//! end supervision over an operator's pause.

use std::{error::Error, path::Path, thread, time::Duration};

use immortal_core::{
    control::{GenerationMatch, Operation, Request, Signal, SignalScope},
    exit::ExitClass,
    status::{LastResult, ServiceState},
};

use crate::control_client::{
    assert_status, request, require_ok, status, wait_for_file, wait_for_state,
};
use crate::fixtures::{ChildGuard, ConfigFile, RuntimeDirectory, spawn_immortal};
use crate::{COMMAND_TIMEOUT, path_str};

/// Readiness timeout of the paused service, long enough for the stop to land
/// before it passes even on a loaded host.
const READINESS_TIMEOUT_SECONDS: u64 = 2;

/// How long the service stays paused. The broker records the readiness failure
/// without publishing it, so nothing observable marks it; this bounded wait,
/// twice the timeout, is what places the failure inside the pause.
const PAUSE: Duration = Duration::from_secs(2 * READINESS_TIMEOUT_SECONDS);

/// Regression: pausing a service that had not declared readiness past its
/// readiness timeout, then continuing it, ended supervision about seven
/// seconds later with "process broker timed out during service readiness".
///
/// The shell stays the main process after writing its start marker, so the
/// stop never reaches a process inside `execve` (see the macOS note in the
/// README).
pub(crate) fn prove_a_pause_outlasting_readiness_fails_readiness_on_continue(
    binary: &Path,
) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("readiness-pause-timeout")?;
    let started = runtime.root().join("service-started");
    let config = ConfigFile::new(
        "readiness-pause-timeout",
        &format!(
            "version: 2\ncommand:\n  - /bin/sh\n  - -c\n  - |\n      : > \"$STARTED\"\n      /bin/sleep 30 &\n      wait\nenvironment:\n  STARTED: '{}'\nreadiness:\n  mode: notify-fd\n  timeout_seconds: {READINESS_TIMEOUT_SECONDS}\nrestart:\n  policy: never\n",
            path_str(&started)?,
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    wait_for_file(&started, COMMAND_TIMEOUT)?;
    let generation = wait_for_state(
        runtime.socket(),
        ServiceState::Running,
        None,
        COMMAND_TIMEOUT,
    )?;

    let signal = |signal| Request {
        operation: Operation::Signal,
        service: runtime.service_name().to_owned(),
        expected_generation: GenerationMatch::Exact(generation),
        scope: SignalScope::Main,
        signal: Some(signal),
    };
    require_ok(
        &request(runtime.socket(), &signal(Signal::Stop))?,
        "job-control stop",
    )?;
    wait_for_state(
        runtime.socket(),
        ServiceState::Paused,
        Some(generation),
        COMMAND_TIMEOUT,
    )?;
    thread::sleep(PAUSE);
    require_ok(
        &request(runtime.socket(), &signal(Signal::Continue))?,
        "job-control continue",
    )?;

    // Readiness failed during the pause, so continuing stops the generation;
    // the restart policy leaves it down.
    wait_for_state(runtime.socket(), ServiceState::Down, None, COMMAND_TIMEOUT).map_err(
        |error| -> Box<dyn Error> {
            if runtime.socket().exists() {
                error
            } else {
                "the supervisor ended supervision after the pause".into()
            }
        },
    )?;
    let last_result = status(runtime.socket())?
        .status
        .ok_or("status payload is absent")?
        .last_result;
    if last_result != Some(LastResult::ReadinessTimeout) {
        return Err(format!("expected a readiness failure, got {last_result:?}").into());
    }

    let halt = request(
        runtime.socket(),
        &Request {
            operation: Operation::Halt,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    require_ok(&halt, "halt after a paused readiness failure")?;
    // The supervisor survived and halted on request; its exit status still
    // reports that the last generation failed readiness.
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::TemporaryFailure,
        "supervisor surviving a pause past its readiness timeout",
    )
}
