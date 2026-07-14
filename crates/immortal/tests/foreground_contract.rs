//! Black-box contracts for the first operational `immortal --foreground` path.

use std::{
    error::Error,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    control::{
        GenerationMatch, Operation, Request, Response, ResponseCode, SignalScope, read_response,
        write_request,
    },
    exit::ExitClass,
    process::{ProcessId, ProcessSignal, SignalTarget, signal as deliver_signal},
    runtime::{CONTROL_SOCKET_NAME, SUPERVISOR_LOCK_NAME},
    status::ServiceState,
};
use tokio::{net::UnixStream, runtime::Builder};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TRUE_PROGRAM: &str = "/usr/bin/true";

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
    let home = TemporaryDirectory::new("real")?;
    let _home_alias = HomeAlias::new(&home.0)?;
    let legacy = ConfigFile::new("legacy-check", "cmd: /bin/true\n")?;
    assert_status(
        run(binary, ["--config", legacy.path_str()?, "--check-config"])?,
        ExitClass::Configuration,
        "unversioned configuration check",
    )?;
    let canonical = ConfigFile::new("canonical-check", "version: 2\ncommand: [/bin/true]\n")?;
    assert_status(
        run(
            binary,
            ["--config", canonical.path_str()?, "--check-config"],
        )?,
        ExitClass::Success,
        "strict version two configuration check",
    )?;
    let success = ConfigFile::new(
        "success",
        "version: 2\ncommand: [/bin/sh, -c, 'exit 0']\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(binary, ["--foreground", "--config", success.path_str()?])?,
        ExitClass::Success,
        "successful foreground configuration",
    )?;
    let runtime_root = home.join(".immortal");
    let runtime_metadata = fs::symlink_metadata(&runtime_root)?;
    if runtime_metadata.permissions().mode() & 0o777 != 0o700 {
        return Err("automatic user runtime root was not created with mode 0700".into());
    }
    let automatic_service = runtime_root.join(success.service_name()?);
    if !automatic_service.join(SUPERVISOR_LOCK_NAME).is_file() {
        return Err("config filename stem did not select the automatic service runtime".into());
    }
    let env_alias = ConfigFile::new(
        "env-alias",
        "version: 2\ncommand: [/bin/sh, -c, 'test \"$DEBUG\" = 1 && test \"$ENVIRONMENT\" = production']\nenv:\n  DEBUG: 1\n  ENVIRONMENT: production\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(binary, ["--foreground", "--config", env_alias.path_str()?])?,
        ExitClass::Success,
        "version two env alias",
    )?;

    let failed_exec = ConfigFile::new(
        "failed-exec",
        "version: 2\ncommand: [/definitely/not/an/immortal-cli-executable]\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(
            binary,
            ["--foreground", "--config", failed_exec.path_str()?],
        )?,
        ExitClass::TemporaryFailure,
        "failed executable",
    )?;

    let readiness = ConfigFile::new(
        "readiness",
        "version: 2\ncommand: [/bin/sh, -c, 'eval \"printf \\\"READY\\\\n\\\" >&$IMMORTAL_READY_FD\"']\nreadiness:\n  mode: notify-fd\n  timeout_seconds: 2\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(binary, ["--foreground", "--config", readiness.path_str()?])?,
        ExitClass::Success,
        "descriptor readiness",
    )?;

    let invalid_readiness = ConfigFile::new(
        "invalid-readiness",
        "version: 2\ncommand: [/bin/sh, -c, 'eval \"printf \\\"WRONG\\\\n\\\" >&$IMMORTAL_READY_FD\"; exec sleep 30']\nreadiness:\n  mode: notify-fd\n  timeout_seconds: 2\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(
            binary,
            ["--foreground", "--config", invalid_readiness.path_str()?],
        )?,
        ExitClass::TemporaryFailure,
        "invalid descriptor readiness",
    )?;

    let readiness_timeout = ConfigFile::new(
        "readiness-timeout",
        "version: 2\ncommand: [/bin/sleep, '30']\nreadiness:\n  mode: notify-fd\n  timeout_seconds: 1\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(
            binary,
            ["--foreground", "--config", readiness_timeout.path_str()?],
        )?,
        ExitClass::TemporaryFailure,
        "descriptor readiness timeout",
    )?;

    let retry_service = runtime_root.join("retry-limit");
    let retry_supervisor = ChildGuard::new(spawn_immortal(
        binary,
        [
            "--foreground",
            "--name",
            "retry-limit",
            "--retries",
            "0",
            TRUE_PROGRAM,
        ],
    )?);
    let retry_socket = retry_service.join(CONTROL_SOCKET_NAME);
    wait_for_childless_state_and_halt(&retry_socket, "retry-limit", ServiceState::Failed)?;
    assert_status(
        retry_supervisor.wait(COMMAND_TIMEOUT).map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!("retry-limited supervisor did not halt: {error}"),
            )
        })?,
        ExitClass::Success,
        "retry-limited supervisor halt",
    )?;
    if !retry_service.join(SUPERVISOR_LOCK_NAME).is_file() {
        return Err("direct service name did not select the automatic service runtime".into());
    }
    assert_status(
        run(binary, ["--foreground", "--name", ".hidden", TRUE_PROGRAM])?,
        ExitClass::Configuration,
        "unsafe direct service name",
    )?;
    let unsafe_stem = ConfigFile::new(
        "unsafe stem",
        "version: 2\ncommand: [/bin/true]\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run(
            binary,
            ["--foreground", "--config", unsafe_stem.path_str()?],
        )?,
        ExitClass::Configuration,
        "unsafe configuration filename stem",
    )?;
    let effective = nix::unistd::geteuid();
    let account = nix::unistd::User::from_uid(effective)?
        .ok_or("effective account is absent from the user database")?;
    let credentialed = ConfigFile::new(
        "credentialed",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'test \"$(id -u)\" = \"$EXPECTED_UID\"']\nuser: {}\nenvironment:\n  EXPECTED_UID: '{}'\nrestart:\n  policy: never\n  exit_when_done: true\n",
            account.name,
            effective.as_raw()
        ),
    )?;
    assert_status(
        run(
            binary,
            ["--foreground", "--config", credentialed.path_str()?],
        )?,
        ExitClass::Success,
        "configured user transition",
    )?;
    let daemon_marker = MarkerFile::new("daemon-ready");
    let daemon = ConfigFile::new(
        "daemon",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'printf ready > \"$MARKER\"']\nenvironment:\n  MARKER: '{}'\nrestart:\n  policy: never\n  exit_when_done: true\n",
            daemon_marker.path_str()?
        ),
    )?;
    assert_status(
        run(binary, ["--config", daemon.path_str()?])?,
        ExitClass::Success,
        "checked daemon launcher",
    )?;
    daemon_marker.wait(COMMAND_TIMEOUT)?;
    assert_status(
        run(
            binary,
            [
                "--config",
                daemon.path_str()?,
                "--control-dir",
                "relative/runtime",
            ],
        )?,
        ExitClass::OsError,
        "checked daemon initialization failure",
    )?;
    assert_status(
        run(binary, ["--log-file", "/tmp/removed.log", TRUE_PROGRAM])?,
        ExitClass::Usage,
        "removed nonoperational option",
    )?;

    let environment = TemporaryDirectory::new("environment")?;
    let environment_marker = MarkerFile::new("environment-loaded");
    fs::write(environment.join("FROM_FILE"), "first\nignored\n")?;
    fs::write(environment.join("EMPTY_VALUE"), "\nignored\n")?;
    fs::write(
        environment.join("MARKER"),
        format!("{}\n", environment_marker.path_str()?),
    )?;
    let environment_supervisor = ChildGuard::new(spawn_immortal(
        binary,
        [
            "--foreground",
            "--name",
            "environment-directory",
            "--retries",
            "0",
            "--env-dir",
            environment.path_str()?,
            "/bin/sh",
            "-c",
            "test \"$FROM_FILE\" = first && test -z \"$EMPTY_VALUE\" && : > \"$MARKER\"",
        ],
    )?);
    if !environment_marker.0.exists() {
        environment_marker.wait(COMMAND_TIMEOUT)?;
    }
    wait_for_childless_state_and_halt(
        &runtime_root
            .join("environment-directory")
            .join(CONTROL_SOCKET_NAME),
        "environment-directory",
        ServiceState::Failed,
    )?;
    assert_status(
        environment_supervisor
            .wait(COMMAND_TIMEOUT)
            .map_err(|error| {
                std::io::Error::new(
                    error.kind(),
                    format!("environment-directory supervisor did not halt: {error}"),
                )
            })?,
        ExitClass::Success,
        "environment-directory supervisor halt",
    )?;
    let missing_environment = environment.join("missing");
    let missing_environment = missing_environment
        .to_str()
        .ok_or("temporary environment path is not UTF-8")?;
    assert_status(
        run(
            binary,
            [
                "--foreground",
                "--name",
                "missing-environment-directory",
                "--env-dir",
                missing_environment,
                TRUE_PROGRAM,
            ],
        )?,
        ExitClass::Configuration,
        "missing direct environment directory",
    )?;

    let supervisor_pid = MarkerFile::new("supervisor-pid");
    let main_pid = MarkerFile::new("main-pid");
    let pid_config = ConfigFile::new(
        "pid-files",
        &format!(
            "version: 2\ncommand: [/bin/sleep, '1']\npid_files:\n  supervisor: '{}'\n  main: '{}'\nrestart:\n  policy: never\n  exit_when_done: true\n",
            supervisor_pid.path_str()?,
            main_pid.path_str()?
        ),
    )?;
    let pid_supervisor = ChildGuard::new(spawn_immortal(
        binary,
        ["--foreground", "--config", pid_config.path_str()?],
    )?);
    supervisor_pid.wait(COMMAND_TIMEOUT)?;
    main_pid.wait(COMMAND_TIMEOUT)?;
    let published_supervisor: u32 = fs::read_to_string(&supervisor_pid.0)?.trim().parse()?;
    let published_main: u32 = fs::read_to_string(&main_pid.0)?.trim().parse()?;
    if published_supervisor != pid_supervisor.id() || published_main == 0 {
        return Err("PID files did not publish the owned supervisor and child".into());
    }
    assert_status(
        pid_supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "PID file lifecycle",
    )?;
    if supervisor_pid.0.exists() || main_pid.0.exists() {
        return Err("owned PID files remained after supervision ended".into());
    }

    let ready = MarkerFile::new("signal-ready");
    let signal_config = ConfigFile::new(
        "signal",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'printf ready > \"$MARKER\"; exec /bin/sleep 30']\nenvironment:\n  MARKER: '{}'\n",
            ready.path_str()?
        ),
    )?;
    let child = spawn_immortal(
        binary,
        ["--foreground", "--config", signal_config.path_str()?],
    )?;
    let child = ChildGuard::new(child);
    ready.wait(COMMAND_TIMEOUT)?;
    let process = ProcessId::new(i32::try_from(child.id())?)
        .ok_or("immortal supervisor PID must be positive")?;
    deliver_signal(SignalTarget::Process(process), ProcessSignal::Terminate)?;
    assert_status(
        child.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "graceful supervisor SIGTERM",
    )?;

    let ready = MarkerFile::new("interrupt-ready");
    let signal_config = ConfigFile::new(
        "interrupt",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'printf ready > \"$MARKER\"; exec /bin/sleep 30']\nenvironment:\n  MARKER: '{}'\n",
            ready.path_str()?
        ),
    )?;
    let child = spawn_immortal(
        binary,
        ["--foreground", "--config", signal_config.path_str()?],
    )?;
    let child = ChildGuard::new(child);
    ready.wait(COMMAND_TIMEOUT)?;
    let process = ProcessId::new(i32::try_from(child.id())?)
        .ok_or("immortal supervisor PID must be positive")?;
    deliver_signal(SignalTarget::Process(process), ProcessSignal::Interrupt)?;
    assert_status(
        child.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "graceful supervisor SIGINT",
    )?;
    prove_direct_logfile_and_logger_receive_both_streams(binary)?;
    prove_logger_exec_permission_denial(binary)?;
    prove_lossless_pipe_backpressure(binary)?;
    prove_logger_drain_timeout_escalates(binary)?;
    prove_unsafe_user_runtime_root_is_rejected(binary)?;
    Ok(())
}

fn prove_direct_logfile_and_logger_receive_both_streams(
    binary: &Path,
) -> Result<(), Box<dyn Error>> {
    let logfile = MarkerFile::new("direct-logfile");
    let logger_output = MarkerFile::new("direct-logger");
    let service_ready = MarkerFile::new("direct-logging-ready");
    let logger_script = format!("cat > '{}'", logger_output.path_str()?);
    let service_script = format!(
        "printf 'direct-stdout\\n'; printf 'direct-stderr\\n' >&2; : > '{}'; exec /bin/sleep 30",
        service_ready.path_str()?
    );
    let supervisor = ChildGuard::new(spawn_immortal(
        binary,
        [
            "--foreground",
            "--name",
            "direct-logging",
            "--logfile",
            logfile.path_str()?,
            "--logger",
            "/bin/sh",
            "-c",
            &logger_script,
            "--",
            "/bin/sh",
            "-c",
            &service_script,
        ],
    )?);
    service_ready.wait(COMMAND_TIMEOUT)?;
    let socket = TemporaryDirectory::test_home_path()
        .join(".immortal")
        .join("direct-logging")
        .join(CONTROL_SOCKET_NAME);
    let status = control_request(
        &socket,
        &Request {
            operation: Operation::Status,
            service: "direct-logging".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    let generation = status
        .generation
        .ok_or("direct logging status omitted the live generation")?;
    let halt = control_request(
        &socket,
        &Request {
            operation: Operation::Halt,
            service: "direct-logging".to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: None,
        },
    )?;
    if halt.code != ResponseCode::Ok {
        return Err(format!("direct logging halt failed: {}", halt.message).into());
    }
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "direct logfile and logger",
    )?;
    let logfile_actual = fs::read_to_string(&logfile.0)?;
    let logger_actual = fs::read_to_string(&logger_output.0)?;
    let mut logfile_lines: Vec<&str> = logfile_actual.lines().collect();
    let mut logger_lines: Vec<&str> = logger_actual.lines().collect();
    logfile_lines.sort_unstable();
    logger_lines.sort_unstable();
    if logfile_lines != ["direct-stderr", "direct-stdout"]
        || logger_lines != ["direct-stderr", "direct-stdout"]
    {
        return Err(format!(
            "direct logging lost or duplicated output: file={logfile_actual:?}, \
             logger={logger_actual:?}"
        )
        .into());
    }
    Ok(())
}

fn prove_unsafe_user_runtime_root_is_rejected(binary: &Path) -> Result<(), Box<dyn Error>> {
    let home = TemporaryDirectory::new("bad")?;
    let root = home.join(".immortal");
    fs::create_dir(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
    let config = ConfigFile::new(
        "bad-root",
        "version: 2\ncommand: [/bin/true]\nrestart:\n  policy: never\n  exit_when_done: true\n",
    )?;
    assert_status(
        run_with_home(
            binary,
            ["--foreground", "--config", config.path_str()?],
            &home.0,
        )?,
        ExitClass::Permission,
        "unsafe automatic user runtime root",
    )
}

fn wait_for_childless_state_and_halt(
    socket: &Path,
    service: &str,
    expected: ServiceState,
) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + COMMAND_TIMEOUT;
    loop {
        if socket.exists() {
            let status = control_request(
                socket,
                &Request {
                    operation: Operation::Status,
                    service: service.to_owned(),
                    expected_generation: GenerationMatch::Any,
                    scope: SignalScope::Main,
                    signal: None,
                },
            )?;
            if status.code == ResponseCode::Ok
                && status.status.as_ref().is_some_and(|snapshot| {
                    snapshot.state == expected && snapshot.main_pid.is_none()
                })
            {
                let halt = control_request(
                    socket,
                    &Request {
                        operation: Operation::Halt,
                        service: service.to_owned(),
                        expected_generation: GenerationMatch::NoChild,
                        scope: SignalScope::Main,
                        signal: None,
                    },
                )?;
                if halt.code == ResponseCode::Ok {
                    return Ok(());
                }
                return Err(format!("halt returned {}: {}", halt.code.name(), halt.message).into());
            }
        }
        if Instant::now() >= deadline {
            return Err(format!("service `{service}` did not reach {expected:?}").into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn control_request(socket: &Path, request: &Request) -> Result<Response, Box<dyn Error>> {
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

fn prove_logger_exec_permission_denial(binary: &Path) -> Result<(), Box<dyn Error>> {
    let logger = MarkerFile::new("logger-no-execute");
    let service = MarkerFile::new("logger-permission-service");
    fs::write(&logger.0, "#!/bin/sh\nexec /bin/cat\n")?;
    fs::set_permissions(&logger.0, fs::Permissions::from_mode(0o600))?;
    let config = ConfigFile::new(
        "logger-permission",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$SERVICE_MARKER\"']\nenvironment:\n  SERVICE_MARKER: '{}'\nlogger: ['{}']\nlogger_restart:\n  max_retries: 0\nrestart:\n  policy: never\n  exit_when_done: true\n",
            service.path_str()?,
            logger.path_str()?
        ),
    )?;
    let supervisor = ChildGuard::new(spawn_immortal(
        binary,
        ["--foreground", "--config", config.path_str()?],
    )?);
    let socket = TemporaryDirectory::test_home_path()
        .join(".immortal")
        .join(config.service_name()?)
        .join(CONTROL_SOCKET_NAME);
    wait_for_childless_state_and_halt(&socket, config.service_name()?, ServiceState::Failed)?;
    assert_status(
        supervisor.wait(COMMAND_TIMEOUT)?,
        ExitClass::Success,
        "non-executable logger supervisor halt",
    )?;
    if service.0.exists() {
        return Err("service started after logger execute permission denial".into());
    }
    Ok(())
}

fn prove_lossless_pipe_backpressure(binary: &Path) -> Result<(), Box<dyn Error>> {
    const PAYLOAD_BYTES: u64 = 16 * 1024 * 1024;

    let writing = MarkerFile::new("backpressure-writing");
    let consuming = MarkerFile::new("backpressure-consuming");
    let finished = MarkerFile::new("backpressure-finished");
    let output = MarkerFile::new("backpressure-output");
    let config = ConfigFile::new(
        "logger-backpressure",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$WRITING\"; dd if=/dev/zero bs=1048576 count=16 2>/dev/null; : > \"$FINISHED\"']\nenvironment:\n  WRITING: '{}'\n  CONSUMING: '{}'\n  FINISHED: '{}'\n  OUTPUT: '{}'\nlogger: [/bin/sh, -c, 'while [ ! -e \"$WRITING\" ]; do sleep 0.01; done; sleep 1; : > \"$CONSUMING\"; cat > \"$OUTPUT\"']\nrestart:\n  policy: never\n  exit_when_done: true\n",
            writing.path_str()?,
            consuming.path_str()?,
            finished.path_str()?,
            output.path_str()?
        ),
    )?;
    assert_status(
        run(binary, ["--foreground", "--config", config.path_str()?]).map_err(|error| {
            std::io::Error::other(format!("logger backpressure contract failed: {error}"))
        })?,
        ExitClass::Success,
        "lossless logger backpressure",
    )?;
    let output_metadata = fs::metadata(&output.0)?;
    let consuming_at = fs::metadata(&consuming.0)?.modified()?;
    let finished_at = fs::metadata(&finished.0)?.modified()?;
    if output_metadata.len() != PAYLOAD_BYTES || finished_at < consuming_at {
        return Err(format!(
            "backpressure contract failed: bytes={}, consuming={consuming_at:?}, finished={finished_at:?}",
            output_metadata.len()
        )
        .into());
    }
    Ok(())
}

fn prove_logger_drain_timeout_escalates(binary: &Path) -> Result<(), Box<dyn Error>> {
    let terminated = MarkerFile::new("logger-drain-terminated");
    let config = ConfigFile::new(
        "logger-drain-timeout",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'printf drain-timeout']\nenvironment:\n  TERM_MARKER: '{}'\nlogger: [/bin/sh, -c, 'trap \": > \\\"$TERM_MARKER\\\"\" TERM; cat >/dev/null; while :; do sleep 30; done']\nrestart:\n  policy: never\n  exit_when_done: true\n",
            terminated.path_str()?
        ),
    )?;
    let started = Instant::now();
    let child = spawn_immortal(binary, ["--foreground", "--config", config.path_str()?])?;
    assert_status(
        ChildGuard::new(child).wait(Duration::from_secs(20))?,
        ExitClass::Success,
        "logger drain timeout escalation",
    )?;
    if !terminated.0.exists() || started.elapsed() < Duration::from_secs(6) {
        return Err("logger was not drained, terminated, and escalated on schedule".into());
    }
    Ok(())
}

#[track_caller]
fn run<'argument>(
    binary: &Path,
    arguments: impl IntoIterator<Item = &'argument str>,
) -> Result<ExitStatus, Box<dyn Error>> {
    let caller = std::panic::Location::caller();
    let child = spawn_immortal(binary, arguments)?;
    ChildGuard::new(child)
        .wait(COMMAND_TIMEOUT)
        .map_err(|error| {
            std::io::Error::new(
                error.kind(),
                format!(
                    "immortal invocation at {}:{} did not finish: {error}",
                    caller.file(),
                    caller.line()
                ),
            )
            .into()
        })
}

fn run_with_home<'argument>(
    binary: &Path,
    arguments: impl IntoIterator<Item = &'argument str>,
    home: &Path,
) -> Result<ExitStatus, Box<dyn Error>> {
    let child = spawn_immortal_with_home(binary, arguments, home)?;
    ChildGuard::new(child)
        .wait(COMMAND_TIMEOUT)
        .map_err(Into::into)
}

fn spawn_immortal<'argument>(
    binary: &Path,
    arguments: impl IntoIterator<Item = &'argument str>,
) -> std::io::Result<Child> {
    spawn_immortal_with_home(binary, arguments, &TemporaryDirectory::test_home_path())
}

fn spawn_immortal_with_home<'argument>(
    binary: &Path,
    arguments: impl IntoIterator<Item = &'argument str>,
    home: &Path,
) -> std::io::Result<Child> {
    Command::new(binary)
        .args(arguments)
        .env("HOME", home)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
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
                    "immortal CLI contract exceeded its deadline",
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

struct ConfigFile(PathBuf);

impl ConfigFile {
    fn new(name: &str, contents: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!(
            "immortal-foreground-{name}-{}-{}.yml",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::write(&path, contents)?;
        Ok(Self(path))
    }

    fn path_str(&self) -> Result<&str, Box<dyn Error>> {
        self.0
            .to_str()
            .ok_or_else(|| "temporary configuration path is not UTF-8".into())
    }

    fn service_name(&self) -> Result<&str, Box<dyn Error>> {
        self.0
            .file_stem()
            .and_then(|name| name.to_str())
            .ok_or_else(|| "temporary configuration stem is not UTF-8".into())
    }
}

impl Drop for ConfigFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new(name: &str) -> std::io::Result<Self> {
        let path = std::env::temp_dir().join(format!("im-fg-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(path))
    }

    fn test_home_path() -> PathBuf {
        std::env::temp_dir().join(format!("im-fg-home-{}", std::process::id()))
    }

    fn path_str(&self) -> Result<&str, Box<dyn Error>> {
        self.0
            .to_str()
            .ok_or_else(|| "temporary directory path is not UTF-8".into())
    }

    fn join(&self, path: impl AsRef<Path>) -> PathBuf {
        self.0.join(path)
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct HomeAlias(PathBuf);

impl HomeAlias {
    fn new(target: &Path) -> std::io::Result<Self> {
        let path = TemporaryDirectory::test_home_path();
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir_all(&path);
        symlink(target, &path)?;
        Ok(Self(path))
    }
}

impl Drop for HomeAlias {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

struct MarkerFile(PathBuf);

impl MarkerFile {
    fn new(name: &str) -> Self {
        let path =
            std::env::temp_dir().join(format!("immortal-foreground-{name}-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        Self(path)
    }

    fn path_str(&self) -> Result<&str, Box<dyn Error>> {
        self.0
            .to_str()
            .ok_or_else(|| "temporary marker path is not UTF-8".into())
    }

    fn wait(&self, timeout: Duration) -> std::io::Result<()> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.0.exists() {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "supervised child did not publish its test marker",
        ))
    }
}

impl Drop for MarkerFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
