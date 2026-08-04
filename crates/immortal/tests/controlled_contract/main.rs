//! Black-box contracts for one authenticated, runtime-owned foreground supervisor.
//!
//! Lifecycle coverage includes final logger EOF drain before Halt lets the
//! supervisor exit.

mod control_client;
mod fixtures;

use std::{error::Error, fs, os::unix::fs::PermissionsExt, path::Path, time::Duration};

use immortal_core::{
    control::{GenerationMatch, Operation, Request, ResponseCode, Signal, SignalScope},
    exit::ExitClass,
    process::{ProcessId, ProcessSignal, SignalTarget, signal as deliver_signal},
    status::{LoggerStatus, ServiceState},
    supervisor::Generation,
};

use self::control_client::{
    assert_status, lifecycle_request, request, require_ok, require_state, status, wait_for_file,
    wait_for_new_ready, wait_for_no_main_pid, wait_for_occurrences, wait_for_state,
    wait_for_state_and_logger,
};
use self::fixtures::{
    ChildGuard, ConfigFile, DetachedProcessGuard, RuntimeDirectory, spawn_immortal,
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
pub(crate) const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Renders a filesystem path as UTF-8 or fails the contract with a clear error.
pub(crate) fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "temporary test path is not UTF-8".into())
}

fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
    prove_controlled_lifecycle(binary)?;
    prove_descriptor_tracking_lifecycle(binary)?;
    prove_explicit_exit_leaves_the_child(binary)?;
    prove_initializing_is_published_until_loggers_are_ready(binary)?;
    prove_direct_logger_permission_denial(binary)?;
    prove_halt_drains_logger(binary)?;
    prove_logger_retry_exhaustion_and_recovery(binary)?;
    prove_readiness_racing_job_control_is_not_fatal(binary)?;
    prove_readiness_racing_a_stop_is_not_fatal(binary)
}

fn prove_halt_drains_logger(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("halt-drain")?;
    let written = runtime.root().join("service-wrote");
    let output = runtime.root().join("logger-output");
    let config = ConfigFile::new(
        "halt-drain",
        &format!(
            "version: 2\ncommand:\n  - /bin/sh\n  - -c\n  - |\n      printf 'drained-before-halt\\n'\n      : > \"$WRITTEN\"\n      exec /bin/sleep 30\nenvironment:\n  WRITTEN: '{}'\nlogger: [/bin/sh, -c, \"cat > '{}'\"]\n",
            path_str(&written)?,
            path_str(&output)?,
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    let generation = wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    wait_for_file(&written, COMMAND_TIMEOUT)?;

    let halt = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Halt,
        generation,
    )?;
    require_ok(&halt, "logger-draining halt")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "logger-draining supervisor halt",
    )?;
    let actual = fs::read_to_string(output)?;
    if actual != "drained-before-halt\n" {
        return Err(format!("Halt returned before logger drain: {actual:?}").into());
    }
    Ok(())
}

fn prove_descriptor_tracking_lifecycle(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new()?;
    let stop = runtime.root().join("stop");
    let stop_allowed = runtime.root().join("stop-allowed");
    let reload = runtime.root().join("reload");
    let reload_allowed = runtime.root().join("reload-allowed");
    let config = ConfigFile::new(
        "descriptor-tracking",
        &format!(
            "version: 2\ncommand:\n  - /bin/sh\n  - -c\n  - |\n      rm -f \"$STOP\"\n      (while [ ! -e \"$STOP\" ]; do /bin/sleep 0.02; done) &\n      exit 0\nenvironment:\n  STOP: '{}'\n  STOP_ALLOWED: '{}'\n  RELOAD: '{}'\n  RELOAD_ALLOWED: '{}'\nrestart:\n  policy: never\nprocess_mode: descriptor-tracking\ndescriptor_tracking:\n  stop:\n    command: [/bin/sh, -c, 'if test -e \"$STOP_ALLOWED\"; then : > \"$STOP\"; else /bin/sleep 3; fi']\n    timeout_seconds: 2\n  reload:\n    command: [/bin/sh, -c, 'test -e \"$RELOAD_ALLOWED\" && : > \"$RELOAD\"']\n    timeout_seconds: 2\n  lifetime_timeout_seconds: 2\n",
            path_str(&stop)?,
            path_str(&stop_allowed)?,
            path_str(&reload)?,
            path_str(&reload_allowed)?,
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    let generation = wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    wait_for_no_main_pid(runtime.socket(), COMMAND_TIMEOUT)?;
    reject_descriptor_direct_control(runtime.socket(), runtime.service_name(), generation)?;

    let failed_reload = request(
        runtime.socket(),
        &Request {
            operation: Operation::Signal,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: Some(Signal::Hangup),
        },
    )?;
    if failed_reload.code != ResponseCode::Internal {
        return Err("failed descriptor reload hook was not reported".into());
    }
    require_state(&status(runtime.socket())?, ServiceState::Ready)?;
    fs::write(&reload_allowed, [])?;
    let reload_response = request(
        runtime.socket(),
        &Request {
            operation: Operation::Signal,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: Some(Signal::Hangup),
        },
    )?;
    require_ok(&reload_response, "descriptor reload hook")?;
    wait_for_file(&reload, COMMAND_TIMEOUT)?;

    let failed_stop = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Stop,
        generation,
    )?;
    if failed_stop.code != ResponseCode::Internal {
        return Err("failed descriptor stop hook was not reported".into());
    }
    require_state(&status(runtime.socket())?, ServiceState::Ready)?;
    fs::write(&stop_allowed, [])?;
    let stop_response = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Stop,
        generation,
    )?;
    require_ok(&stop_response, "descriptor stop hook")?;
    wait_for_state(runtime.socket(), ServiceState::Down, None, COMMAND_TIMEOUT)?;
    let snapshot = status(runtime.socket())?
        .status
        .ok_or("descriptor Down status payload is absent")?;
    if snapshot.main_pid.is_some()
        || snapshot.last_result != Some(immortal_core::status::LastResult::LifetimeClosed)
    {
        return Err(format!("invalid descriptor Down status: {snapshot:?}").into());
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
    require_ok(&halt, "descriptor supervisor halt")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "descriptor supervisor completion",
    )
}

fn reject_descriptor_direct_control(
    socket: &Path,
    service: &str,
    generation: Generation,
) -> Result<(), Box<dyn Error>> {
    let raw_signal = request(
        socket,
        &Request {
            operation: Operation::Signal,
            service: service.to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Group,
            signal: Some(Signal::User1),
        },
    )?;
    if raw_signal.code != ResponseCode::Invalid {
        return Err("descriptor-tracked raw signal was not rejected".into());
    }
    let detach = lifecycle_request(socket, service, Operation::Exit, generation)?;
    if detach.code != ResponseCode::Invalid {
        return Err("descriptor-tracked supervisor detach was not rejected".into());
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn prove_controlled_lifecycle(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new()?;
    let marker = runtime.root().join("events");
    let config = ConfigFile::new(
        "controlled",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'trap \"printf usr1\\n >> \\\"$MARKER\\\"\" USR1; trap \"exit 0\" TERM; printf start\\n >> \"$MARKER\"; while :; do sleep 1; done']\nenvironment:\n  MARKER: '{}'\nlogger: [/bin/cat]\n",
            path_str(&marker)?
        ),
    )?;

    let child = spawn_immortal(binary, &config, runtime.service())?;
    let child = ChildGuard::new(child);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    wait_for_occurrences(&marker, "start", 1, COMMAND_TIMEOUT)?;

    let initial_status = status(runtime.socket())?;
    let first = require_state(&initial_status, ServiceState::Ready)?;
    let snapshot = initial_status
        .status
        .as_ref()
        .ok_or("status payload is absent")?;
    if snapshot.supervisor_pid != Some(child.id())
        || snapshot.main_pid.is_none()
        || snapshot.starts != 1
        || snapshot.failures != 0
        || snapshot.logger != LoggerStatus::Ready
        || snapshot.command.first().map(String::as_str) != Some("/bin/sh")
    {
        return Err(format!("incomplete initial runtime status: {snapshot:?}").into());
    }
    let wrong_service = request(
        runtime.socket(),
        &Request {
            operation: Operation::Status,
            service: "another-service".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    if wrong_service.code != ResponseCode::NotFound {
        return Err("service-name mismatch was not rejected".into());
    }
    let stale =
        Generation::new(first.get().saturating_add(10)).ok_or("invalid stale generation")?;
    let stale_stop = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Stop,
        stale,
    )?;
    if stale_stop.code != ResponseCode::Conflict {
        return Err("stale generation mutation was not rejected".into());
    }

    let signal = request(
        runtime.socket(),
        &Request {
            operation: Operation::Signal,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::Exact(first),
            scope: SignalScope::Main,
            signal: Some(Signal::User1),
        },
    )?;
    require_ok(&signal, "USR1 delivery")?;
    wait_for_occurrences(&marker, "usr1", 1, COMMAND_TIMEOUT)?;
    prove_signal_burst_remains_generation_bound(runtime.socket(), runtime.service_name(), first)?;

    let stop = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Stop,
        first,
    )?;
    require_ok(&stop, "stop")?;
    wait_for_state(runtime.socket(), ServiceState::Down, None, COMMAND_TIMEOUT)?;
    let down = status(runtime.socket())?;
    let down = down
        .status
        .as_ref()
        .ok_or("down status payload is absent")?;
    if down.main_pid.is_some()
        || down.starts != 1
        || down.failures != 0
        || down.logger != LoggerStatus::Ready
        || down.last_result.is_none()
        || down.down_seconds.is_none()
    {
        return Err(format!("incomplete down runtime status: {down:?}").into());
    }

    let duplicate = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    assert_status(
        duplicate.wait(COMMAND_TIMEOUT)?,
        ExitClass::OsError,
        "duplicate runtime owner",
    )?;

    let once = request(
        runtime.socket(),
        &Request {
            operation: Operation::Once,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    require_ok(&once, "once")?;
    let once_generation =
        wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    wait_for_occurrences(&marker, "start", 2, COMMAND_TIMEOUT)?;
    let finish_once = request(
        runtime.socket(),
        &Request {
            operation: Operation::Signal,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::Exact(once_generation),
            scope: SignalScope::Main,
            signal: Some(Signal::Kill),
        },
    )?;
    require_ok(&finish_once, "once termination")?;
    wait_for_state(runtime.socket(), ServiceState::Down, None, COMMAND_TIMEOUT)?;

    let start = request(
        runtime.socket(),
        &Request {
            operation: Operation::Start,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    require_ok(&start, "start")?;
    let second = wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    if second == first {
        return Err("start reused the previous generation".into());
    }
    if status(runtime.socket())?
        .status
        .is_none_or(|status| status.starts != 3)
    {
        return Err("start count was not published after manual start".into());
    }
    wait_for_occurrences(&marker, "start", 3, COMMAND_TIMEOUT)?;

    let restart = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Restart,
        second,
    )?;
    require_ok(&restart, "restart")?;
    let third = wait_for_new_ready(runtime.socket(), second, COMMAND_TIMEOUT)?;
    if status(runtime.socket())?
        .status
        .is_none_or(|status| status.starts != 4)
    {
        return Err("start count was not published after restart".into());
    }
    wait_for_occurrences(&marker, "start", 4, COMMAND_TIMEOUT)?;

    let unsafe_exit = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Exit,
        third,
    )?;
    if unsafe_exit.code != ResponseCode::Invalid {
        return Err("logged service exit did not reject incomplete graph detachment".into());
    }
    wait_for_state(
        runtime.socket(),
        ServiceState::Ready,
        Some(third),
        COMMAND_TIMEOUT,
    )?;

    let halt = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Halt,
        third,
    )?;
    require_ok(&halt, "halt")?;
    assert_status(
        child.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "controlled supervisor halt",
    )?;
    if runtime.socket().exists() {
        return Err("control socket remained after supervisor exit".into());
    }

    Ok(())
}

fn prove_signal_burst_remains_generation_bound(
    socket: &Path,
    service: &str,
    generation: Generation,
) -> Result<(), Box<dyn Error>> {
    let signal = Request {
        operation: Operation::Signal,
        service: service.to_owned(),
        expected_generation: GenerationMatch::Exact(generation),
        scope: SignalScope::Main,
        signal: Some(Signal::WindowChange),
    };
    for _request in 0..32 {
        let response = request(socket, &signal)?;
        require_ok(&response, "generation-bound signal burst")?;
    }
    let response = status(socket)?;
    if require_state(&response, ServiceState::Ready)? != generation {
        return Err("signal burst changed the live generation".into());
    }
    Ok(())
}

fn prove_explicit_exit_leaves_the_child(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("detached")?;
    let pid_file = runtime.root().join("child.pid");
    let config = ConfigFile::new(
        "detached",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'printf %s \"$$\" > \"$PID_FILE\"; exec /bin/sleep 30']\nenvironment:\n  PID_FILE: '{}'\n",
            path_str(&pid_file)?
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    wait_for_file(&pid_file, COMMAND_TIMEOUT)?;
    let status = status(runtime.socket())?;
    let generation = require_state(&status, ServiceState::Ready)?;
    let exit = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Exit,
        generation,
    )?;
    require_ok(&exit, "exit")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "supervisor exit with live child",
    )?;

    let pid: i32 = fs::read_to_string(&pid_file)?.trim().parse()?;
    let process = ProcessId::try_from(pid)?;
    let mut cleanup = DetachedProcessGuard::new(process);
    deliver_signal(SignalTarget::Process(process), ProcessSignal::Continue)?;
    cleanup.kill();
    Ok(())
}

fn prove_logger_retry_exhaustion_and_recovery(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("logger-recovery")?;
    let logger = runtime.root().join("logger");
    let attempts = runtime.root().join("logger-attempts");
    let allow = runtime.root().join("logger-allowed");
    let service_marker = runtime.root().join("service-started");
    fs::write(
        &logger,
        format!(
            "#!/bin/sh\nprintf x >> '{}'\n[ -e '{}' ] || exit 23\nexec /bin/cat\n",
            path_str(&attempts)?,
            path_str(&allow)?
        ),
    )?;
    fs::set_permissions(&logger, fs::Permissions::from_mode(0o755))?;
    let config = ConfigFile::new(
        "logger-recovery",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$SERVICE_MARKER\"; exec /bin/sleep 30']\nstart_delay_seconds: 5\nenvironment:\n  SERVICE_MARKER: '{}'\nlogger: ['{}']\nlogger_restart:\n  max_retries: 1\n  backoff:\n    initial_seconds: 1\n    max_seconds: 1\n    multiplier: 1\n    jitter_percent: 0\n    reset_after_seconds: 60\n",
            path_str(&service_marker)?,
            path_str(&logger)?
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    wait_for_state(
        runtime.socket(),
        ServiceState::Failed,
        None,
        COMMAND_TIMEOUT,
    )?;

    let failed = status(runtime.socket())?;
    let snapshot = failed.status.as_ref().ok_or("failed status is absent")?;
    if snapshot.logger != LoggerStatus::Failed
        || snapshot.starts != 0
        || service_marker.exists()
        || fs::read_to_string(&attempts)? != "xx"
    {
        return Err(format!("invalid exhausted logger status: {snapshot:?}").into());
    }

    fs::write(&allow, [])?;
    let start = request(
        runtime.socket(),
        &Request {
            operation: Operation::Start,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    require_ok(&start, "logger recovery start")?;
    let generation = wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    wait_for_file(&service_marker, COMMAND_TIMEOUT)?;
    let recovered = status(runtime.socket())?;
    if recovered
        .status
        .as_ref()
        .is_none_or(|status| status.logger != LoggerStatus::Ready || status.starts != 1)
    {
        return Err("manual start did not recover the logger pipeline".into());
    }

    let halt = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Halt,
        generation,
    )?;
    require_ok(&halt, "logger recovery halt")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "recovered logger supervisor halt",
    )
}

fn prove_direct_logger_permission_denial(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("logger-permission")?;
    let logger = runtime.root().join("logger");
    let service_marker = runtime.root().join("service-started");
    fs::write(&logger, "#!/bin/sh\nexec /bin/cat\n")?;
    fs::set_permissions(&logger, fs::Permissions::from_mode(0o600))?;
    let config = ConfigFile::new(
        "logger-permission",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$SERVICE_MARKER\"']\nenvironment:\n  SERVICE_MARKER: '{}'\nlogger: ['{}']\nlogger_restart:\n  max_retries: 0\n",
            path_str(&service_marker)?,
            path_str(&logger)?
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    wait_for_state(
        runtime.socket(),
        ServiceState::Failed,
        None,
        COMMAND_TIMEOUT,
    )?;
    let failed = status(runtime.socket())?;
    if failed.status.as_ref().is_none_or(|status| {
        status.logger != LoggerStatus::Failed || status.starts != 0 || service_marker.exists()
    }) {
        return Err("direct logger permission denial was not published".into());
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
    require_ok(&halt, "permission-denied logger halt")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "permission-denied logger supervisor halt",
    )
}

fn prove_initializing_is_published_until_loggers_are_ready(
    binary: &Path,
) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("initializing")?;
    let logger = runtime.root().join("delayed-logger");
    let config = ConfigFile::new(
        "initializing",
        &format!(
            "version: 2\ncommand: [/bin/sleep, '30']\nlogger: ['{}']\nlogger_restart:\n  backoff:\n    initial_seconds: 2\n    max_seconds: 2\n    multiplier: 1\n    jitter_percent: 0\n    reset_after_seconds: 60\n",
            path_str(&logger)?
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(binary, &config, runtime.service())?);
    runtime.wait_for_socket(COMMAND_TIMEOUT)?;
    let initializing = wait_for_state_and_logger(
        runtime.socket(),
        ServiceState::Initializing,
        LoggerStatus::Backoff,
        COMMAND_TIMEOUT,
    )?;
    if initializing
        .status
        .as_ref()
        .is_none_or(|status| status.main_pid.is_some() || status.starts != 0)
    {
        return Err("initializing status claimed a service generation".into());
    }

    fs::write(&logger, "#!/bin/sh\nexec /bin/cat\n")?;
    fs::set_permissions(&logger, fs::Permissions::from_mode(0o755))?;
    let generation = wait_for_state(runtime.socket(), ServiceState::Ready, None, COMMAND_TIMEOUT)?;
    let halt = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Halt,
        generation,
    )?;
    require_ok(&halt, "initializing supervisor halt")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "initializing supervisor completion",
    )
}

/// Regression: readiness arriving while the generation is paused killed the supervisor.
///
/// The broker's readiness watcher forwards on generation membership alone, so
/// it is independent of supervisor state. Guarding the executor arm on
/// `Running` routed the observation to `UnexpectedBrokerEvent`, which exited
/// the supervisor and made the broker kill the service.
///
/// The declaration runs in a background subshell sharing the inherited
/// readiness descriptor, so job control applied to the main process alone
/// cannot prevent it. Resuming must then observe `Ready`, proving the
/// observation was recorded against the paused generation rather than dropped.
fn prove_readiness_racing_job_control_is_not_fatal(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("readiness-paused")?;
    let release = runtime.root().join("release-readiness");
    let started = runtime.root().join("service-started");
    let config = ConfigFile::new(
        "readiness-paused",
        &format!(
            "version: 2\ncommand:\n  - /bin/sh\n  - -c\n  - |\n      (while [ ! -e \"$RELEASE\" ]; do /bin/sleep 0.02; done; eval \"printf 'READY\\n' >&$IMMORTAL_READY_FD\") &\n      : > \"$STARTED\"\n      exec /bin/sleep 30\nenvironment:\n  RELEASE: '{}'\n  STARTED: '{}'\nreadiness:\n  mode: notify-fd\n  timeout_seconds: 60\nrestart:\n  policy: never\n",
            path_str(&release)?,
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

    let paused = request(
        runtime.socket(),
        &Request {
            operation: Operation::Signal,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: Some(Signal::Stop),
        },
    )?;
    require_ok(&paused, "job-control stop")?;
    wait_for_state(
        runtime.socket(),
        ServiceState::Paused,
        Some(generation),
        COMMAND_TIMEOUT,
    )?;

    fs::write(&release, [])?;
    let resumed = request(
        runtime.socket(),
        &Request {
            operation: Operation::Signal,
            service: runtime.service_name().to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: Some(Signal::Continue),
        },
    )?;
    require_ok(&resumed, "job-control continue")?;
    wait_for_state(
        runtime.socket(),
        ServiceState::Ready,
        Some(generation),
        COMMAND_TIMEOUT,
    )?;

    let halt = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Halt,
        generation,
    )?;
    require_ok(&halt, "halt after paused readiness")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "supervisor surviving readiness during job control",
    )
}

/// Regression: readiness arriving while the generation is stopping killed the supervisor.
///
/// The service traps `SIGTERM` and holds the stop open, giving a deterministic
/// `Stopping` window. Its readiness writer ignores `SIGTERM` so the
/// declaration is guaranteed to arrive inside that window rather than racing
/// group termination.
fn prove_readiness_racing_a_stop_is_not_fatal(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("readiness-stopping")?;
    let exit = runtime.root().join("service-may-exit");
    let notified = runtime.root().join("readiness-sent");
    let release = runtime.root().join("release-readiness");
    let started = runtime.root().join("service-started");
    let stopping = runtime.root().join("service-stopping");
    let config = ConfigFile::new(
        "readiness-stopping",
        &format!(
            "version: 2\ncommand:\n  - /bin/sh\n  - -c\n  - |\n      trap ': > \"$STOPPING\"; while [ ! -e \"$EXIT\" ]; do /bin/sleep 0.02; done; exit 0' TERM\n      (trap '' TERM; while [ ! -e \"$RELEASE\" ]; do /bin/sleep 0.02; done; eval \"printf 'READY\\n' >&$IMMORTAL_READY_FD\"; : > \"$NOTIFIED\") &\n      : > \"$STARTED\"\n      /bin/sleep 30 &\n      wait\nenvironment:\n  EXIT: '{}'\n  NOTIFIED: '{}'\n  RELEASE: '{}'\n  STARTED: '{}'\n  STOPPING: '{}'\nreadiness:\n  mode: notify-fd\n  timeout_seconds: 60\nrestart:\n  policy: never\n",
            path_str(&exit)?,
            path_str(&notified)?,
            path_str(&release)?,
            path_str(&started)?,
            path_str(&stopping)?,
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

    let stop = lifecycle_request(
        runtime.socket(),
        runtime.service_name(),
        Operation::Stop,
        generation,
    )?;
    require_ok(&stop, "stop before readiness")?;
    wait_for_file(&stopping, COMMAND_TIMEOUT)?;

    // The generation is stopping now, so the declaration below is observed
    // outside `Running`. The stop grace period bounds this window, so the
    // child is released as soon as the declaration has been written.
    fs::write(&release, [])?;
    wait_for_file(&notified, COMMAND_TIMEOUT)?;
    fs::write(&exit, [])?;
    wait_for_state(runtime.socket(), ServiceState::Down, None, COMMAND_TIMEOUT)?;

    // The stopped generation is gone, so the final halt binds to the absence
    // of a child rather than to a generation.
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
    require_ok(&halt, "halt after stopped readiness")?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "supervisor surviving readiness during stop",
    )
}
