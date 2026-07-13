//! Black-box contracts for one authenticated, runtime-owned foreground supervisor.
//!
//! Lifecycle coverage includes final logger EOF drain before Halt lets the
//! supervisor exit.

use std::{
    error::Error,
    fs::{self, File},
    io::Read,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    control::{
        GenerationMatch, Operation, Request, Response, ResponseCode, Signal, SignalScope,
        read_response, write_request,
    },
    exit::ExitClass,
    process::{ProcessGroupId, ProcessSignal, SignalTarget, signal as deliver_signal},
    status::{LoggerStatus, ServiceState},
    supervisor::Generation,
};
use tokio::{net::UnixStream, runtime::Builder};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const DIAGNOSTIC_LIMIT: u64 = 16 * 1024;
const POLL_INTERVAL: Duration = Duration::from_millis(20);
static RUNTIME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
    prove_controlled_lifecycle(binary)?;
    prove_descriptor_tracking_lifecycle(binary)?;
    prove_explicit_exit_leaves_the_child(binary)?;
    prove_initializing_is_published_until_loggers_are_ready(binary)?;
    prove_direct_logger_permission_denial(binary)?;
    prove_halt_drains_logger(binary)?;
    prove_logger_retry_exhaustion_and_recovery(binary)
}

fn prove_halt_drains_logger(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new_named("halt-drain")?;
    let written = runtime.root().join("service-wrote");
    let output = runtime.root().join("logger-output");
    let config = ConfigFile::new(
        "halt-drain",
        &format!(
            "version: 2\ncommand:\n  - /bin/sh\n  - -c\n  - |\n      printf 'drained-before-halt\\n'\n      : > \"$WRITTEN\"\n      exec /bin/sleep 30\nenvironment:\n  WRITTEN: '{}'\nlogging:\n  combine_stderr: true\n  stdout:\n    logger: [/bin/sh, -c, \"cat > '{}'\"]\n",
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
            "version: 2\ncommand: [/bin/sh, -c, 'trap \"printf usr1\\n >> \\\"$MARKER\\\"\" USR1; trap \"exit 0\" TERM; printf start\\n >> \"$MARKER\"; while :; do sleep 1; done']\nenvironment:\n  MARKER: '{}'\nlogging:\n  stdout:\n    logger: [/bin/cat]\n",
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
    wait_for_occurrences(&marker, "start", 2, COMMAND_TIMEOUT)?;

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
    let group = ProcessGroupId::try_from(pid)?;
    let mut cleanup = DetachedGroupGuard::new(group);
    deliver_signal(SignalTarget::Group(group), ProcessSignal::Continue)?;
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
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$SERVICE_MARKER\"; exec /bin/sleep 30']\nstart_delay_seconds: 5\nenvironment:\n  SERVICE_MARKER: '{}'\nlogging:\n  combine_stderr: true\n  restart:\n    max_retries: 1\n    backoff:\n      initial_seconds: 1\n      max_seconds: 1\n      multiplier: 1\n      jitter_percent: 0\n      reset_after_seconds: 60\n  stdout:\n    logger: ['{}']\n",
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
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$SERVICE_MARKER\"']\nenvironment:\n  SERVICE_MARKER: '{}'\nlogging:\n  combine_stderr: true\n  restart:\n    max_retries: 0\n  stdout:\n    logger: ['{}']\n",
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
            "version: 2\ncommand: [/bin/sleep, '30']\nlogging:\n  combine_stderr: true\n  restart:\n    backoff:\n      initial_seconds: 2\n      max_seconds: 2\n      multiplier: 1\n      jitter_percent: 0\n      reset_after_seconds: 60\n  stdout:\n    logger: ['{}']\n",
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

fn spawn_immortal(
    binary: &Path,
    config: &ConfigFile,
    service: &Path,
) -> Result<Child, Box<dyn Error>> {
    let root = service
        .parent()
        .ok_or("test service directory has no runtime root")?;
    let diagnostics = File::create(root.join("immortal.stderr"))?;
    Ok(Command::new(binary)
        .args([
            "--foreground",
            "--config",
            config.path_str()?,
            "--control-dir",
            path_str(service)?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostics))
        .spawn()?)
}

fn status(socket: &Path) -> Result<Response, Box<dyn Error>> {
    request(
        socket,
        &Request {
            operation: Operation::Status,
            service: socket
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .ok_or("invalid test service path")?
                .to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        },
    )
}

fn lifecycle_request(
    socket: &Path,
    service: &str,
    operation: Operation,
    generation: Generation,
) -> Result<Response, Box<dyn Error>> {
    request(
        socket,
        &Request {
            operation,
            service: service.to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: None,
        },
    )
}

fn request(socket: &Path, request: &Request) -> Result<Response, Box<dyn Error>> {
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut stream = UnixStream::connect(socket).await?;
        write_request(&mut stream, request).await?;
        read_response(&mut stream).await.map_err(Into::into)
    })
}

fn require_ok(response: &Response, operation: &str) -> Result<(), Box<dyn Error>> {
    if response.code == ResponseCode::Ok {
        Ok(())
    } else {
        Err(format!(
            "{operation} returned {}: {}",
            response.code.name(),
            response.message
        )
        .into())
    }
}

fn require_state(
    response: &Response,
    expected: ServiceState,
) -> Result<Generation, Box<dyn Error>> {
    require_ok(response, "status")?;
    let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
    if snapshot.state != expected {
        return Err(format!("expected state {expected:?}, received {:?}", snapshot.state).into());
    }
    response
        .generation
        .ok_or_else(|| "status generation is absent".into())
}

fn wait_for_state(
    socket: &Path,
    expected: ServiceState,
    generation: Option<Generation>,
    timeout: Duration,
) -> Result<Generation, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
        if snapshot.state == expected
            && generation.is_none_or(|generation| response.generation == Some(generation))
        {
            return Ok(response.generation.unwrap_or(Generation::FIRST));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "service did not reach {expected:?}; last status was {snapshot:?}, generation {:?}",
                response.generation
            )
            .into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_no_main_pid(socket: &Path, timeout: Duration) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        require_ok(&response, "descriptor status")?;
        if response
            .status
            .as_ref()
            .is_some_and(|snapshot| snapshot.main_pid.is_none())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("descriptor launcher PID remained published".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_state_and_logger(
    socket: &Path,
    expected_state: ServiceState,
    expected_logger: LoggerStatus,
    timeout: Duration,
) -> Result<Response, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
        if snapshot.state == expected_state && snapshot.logger == expected_logger {
            return Ok(response);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "service did not reach {expected_state:?}/{expected_logger:?}; last status was {snapshot:?}"
            )
            .into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_new_ready(
    socket: &Path,
    previous: Generation,
    timeout: Duration,
) -> Result<Generation, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
        if snapshot.state == ServiceState::Ready
            && response
                .generation
                .is_some_and(|generation| generation != previous)
        {
            return response
                .generation
                .ok_or_else(|| "ready generation is absent".into());
        }
        if Instant::now() >= deadline {
            return Err("service did not publish a replacement ready generation".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn wait_for_occurrences(
    path: &Path,
    pattern: &str,
    count: usize,
    timeout: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if fs::read_to_string(path).is_ok_and(|contents| contents.matches(pattern).count() >= count)
        {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("marker did not contain {count} occurrences of {pattern:?}"),
    ))
}

fn wait_for_file(path: &Path, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "child PID file was not published",
    ))
}

fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "temporary test path is not UTF-8".into())
}

fn assert_status(
    status: ExitStatus,
    expected: ExitClass,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    if status.code() == Some(i32::from(expected.value())) {
        Ok(())
    } else {
        Err(format!(
            "{context} returned {status}; expected exit {}",
            expected.value()
        )
        .into())
    }
}

struct RuntimeDirectory {
    diagnostics: PathBuf,
    name: String,
    root: PathBuf,
    service: PathBuf,
    socket: PathBuf,
}

impl RuntimeDirectory {
    fn new() -> std::io::Result<Self> {
        Self::new_named("api")
    }

    fn new_named(service_name: &str) -> std::io::Result<Self> {
        let root = Path::new("/tmp").join(format!(
            "immortal-{}-{}",
            std::process::id(),
            RUNTIME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        let root = fs::canonicalize(root)?;
        let service = root.join(service_name);
        let socket = service.join("immortal.sock");
        let diagnostics = root.join("immortal.stderr");
        Ok(Self {
            diagnostics,
            name: service_name.to_owned(),
            root,
            service,
            socket,
        })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn service(&self) -> &Path {
        &self.service
    }

    fn service_name(&self) -> &str {
        &self.name
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn wait_for_socket(&self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.socket().exists() {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
        let diagnostics = read_diagnostics(&self.diagnostics)
            .unwrap_or_else(|error| format!("diagnostics unavailable: {error}"));
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            format!("control socket was not created; supervisor diagnostics: {diagnostics}"),
        ))
    }
}

fn read_diagnostics(path: &Path) -> std::io::Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(DIAGNOSTIC_LIMIT)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct ConfigFile(PathBuf);

impl ConfigFile {
    fn new(name: &str, contents: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "immortal-{name}-{}-{}.yml",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::write(&path, contents)?;
        Ok(Self(path))
    }

    fn path_str(&self) -> Result<&str, Box<dyn Error>> {
        path_str(&self.0)
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    const fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    fn id(&self) -> u32 {
        self.child.id()
    }

    fn wait(mut self, timeout: Duration) -> std::io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _ = self.child.wait();
                self.reaped = true;
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "controlled immortal contract exceeded its deadline",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct DetachedGroupGuard {
    group: ProcessGroupId,
    killed: bool,
}

impl DetachedGroupGuard {
    const fn new(group: ProcessGroupId) -> Self {
        Self {
            group,
            killed: false,
        }
    }

    fn kill(&mut self) {
        let _ = deliver_signal(SignalTarget::Group(self.group), ProcessSignal::Kill);
        self.killed = true;
    }
}

impl Drop for DetachedGroupGuard {
    fn drop(&mut self) {
        if !self.killed {
            let _ = deliver_signal(SignalTarget::Group(self.group), ProcessSignal::Kill);
        }
    }
}
