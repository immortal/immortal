//! Black-box contracts for the first operational `immortal --foreground` path.

use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    exit::ExitClass,
    process::{ProcessId, ProcessSignal, SignalTarget, signal as deliver_signal},
};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const TRUE_PROGRAM: &str = "/usr/bin/true";

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
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

    assert_status(
        run(binary, ["--foreground", "--retries", "0", TRUE_PROGRAM])?,
        ExitClass::TemporaryFailure,
        "retry limit",
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
    prove_logger_exec_permission_denial(binary)?;
    prove_lossless_pipe_backpressure(binary)?;
    prove_logger_drain_timeout_escalates(binary)?;
    Ok(())
}

fn prove_logger_exec_permission_denial(binary: &Path) -> Result<(), Box<dyn Error>> {
    let logger = MarkerFile::new("logger-no-execute");
    let service = MarkerFile::new("logger-permission-service");
    fs::write(&logger.0, "#!/bin/sh\nexec /bin/cat\n")?;
    fs::set_permissions(&logger.0, fs::Permissions::from_mode(0o600))?;
    let config = ConfigFile::new(
        "logger-permission",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$SERVICE_MARKER\"']\nenvironment:\n  SERVICE_MARKER: '{}'\nlogging:\n  combine_stderr: true\n  restart:\n    max_retries: 0\n  stdout:\n    logger: ['{}']\nrestart:\n  policy: never\n  exit_when_done: true\n",
            service.path_str()?,
            logger.path_str()?
        ),
    )?;
    assert_status(
        run(binary, ["--foreground", "--config", config.path_str()?])?,
        ExitClass::TemporaryFailure,
        "non-executable logger",
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
            "version: 2\ncommand: [/bin/sh, -c, ': > \"$WRITING\"; dd if=/dev/zero bs=1048576 count=16 2>/dev/null; : > \"$FINISHED\"']\nenvironment:\n  WRITING: '{}'\n  CONSUMING: '{}'\n  FINISHED: '{}'\n  OUTPUT: '{}'\nlogging:\n  combine_stderr: true\n  stdout:\n    logger: [/bin/sh, -c, 'while [ ! -e \"$WRITING\" ]; do sleep 0.01; done; sleep 1; : > \"$CONSUMING\"; cat > \"$OUTPUT\"']\nrestart:\n  policy: never\n  exit_when_done: true\n",
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
            "version: 2\ncommand: [/bin/sh, -c, 'printf drain-timeout']\nenvironment:\n  TERM_MARKER: '{}'\nlogging:\n  combine_stderr: true\n  stdout:\n    logger: [/bin/sh, -c, 'trap \": > \\\"$TERM_MARKER\\\"\" TERM; cat >/dev/null; while :; do sleep 30; done']\nrestart:\n  policy: never\n  exit_when_done: true\n",
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

fn run<'argument>(
    binary: &Path,
    arguments: impl IntoIterator<Item = &'argument str>,
) -> Result<ExitStatus, Box<dyn Error>> {
    let child = spawn_immortal(binary, arguments)?;
    ChildGuard::new(child)
        .wait(COMMAND_TIMEOUT)
        .map_err(Into::into)
}

fn spawn_immortal<'argument>(
    binary: &Path,
    arguments: impl IntoIterator<Item = &'argument str>,
) -> std::io::Result<Child> {
    Command::new(binary)
        .args(arguments)
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
}

impl Drop for ConfigFile {
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
