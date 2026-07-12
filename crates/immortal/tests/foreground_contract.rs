//! Black-box contracts for the first operational `immortal --foreground` path.

use std::{
    error::Error,
    fs,
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

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
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
        run(binary, ["--foreground", "--retries", "0", "/bin/true"])?,
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
        run(
            binary,
            [
                "--foreground",
                "--log-file",
                "/tmp/immortal-unimplemented.log",
                "/bin/true",
            ],
        )?,
        ExitClass::Unavailable,
        "gated logger option",
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
