//! Black-box contract for exhaustive control-operation routing.

use std::{
    error::Error,
    ffi::OsStr,
    fs,
    io::{self, Read},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Output, Stdio},
    sync::atomic::{AtomicU64, Ordering},
    thread,
    time::{Duration, Instant},
};

use immortal_core::exit::ExitClass;

const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortalctl"));
    let runtime = TestDirectory::new()?;

    prove_startup_routes(binary, runtime.path())?;
    prove_control_routes(binary, runtime.path())
}

fn prove_startup_routes(binary: &Path, runtime: &Path) -> Result<(), Box<dyn Error>> {
    let help = run(binary, &[OsStr::new("--help")])?;
    require_status(help.status, ExitClass::Success, "help")?;

    // `-h` was the Go alias for `hup`, so asking the installed binary for help
    // used to signal a named service instead. It must display help and reach
    // no supervisor at all.
    let short_help = run(binary, &[OsStr::new("-h"), OsStr::new("api")])?;
    require_status(short_help.status, ExitClass::Success, "short help")?;
    if !String::from_utf8_lossy(&short_help.stdout).contains("Usage: immortalctl") {
        return Err("-h must display help rather than signal a service".into());
    }

    let invalid = run(binary, &[OsStr::new("--definitely-invalid")])?;
    require_status(invalid.status, ExitClass::Usage, "invalid arguments")?;

    let status = run(binary, &[OsStr::new("--runtime-dir"), runtime.as_os_str()])?;
    require_status(status.status, ExitClass::Success, "default status route")
}

fn prove_control_routes(binary: &Path, runtime: &Path) -> Result<(), Box<dyn Error>> {
    for operation in ["start", "stop", "restart", "once", "exit", "halt"] {
        let output = run(
            binary,
            &[
                OsStr::new("--runtime-dir"),
                runtime.as_os_str(),
                OsStr::new(operation),
                OsStr::new("missing"),
            ],
        )?;
        require_status(output.status, ExitClass::NotFound, operation)?;
    }

    let signal = run(
        binary,
        &[
            OsStr::new("--runtime-dir"),
            runtime.as_os_str(),
            OsStr::new("signal"),
            OsStr::new("term"),
            OsStr::new("missing"),
        ],
    )?;
    require_status(signal.status, ExitClass::NotFound, "signal")
}

fn run(binary: &Path, arguments: &[&OsStr]) -> Result<Output, Box<dyn Error>> {
    let child = Command::new(binary)
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    ChildGuard::new(child)
        .wait_with_output(COMMAND_TIMEOUT)
        .map_err(Into::into)
}

fn require_status(
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

    fn wait_with_output(mut self, timeout: Duration) -> io::Result<Output> {
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = self.child.try_wait()? {
                self.reaped = true;
                break status;
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _status = self.child.wait()?;
                self.reaped = true;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "immortalctl routing contract exceeded its deadline",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        };

        let mut stdout = Vec::new();
        self.child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("control command stdout was not piped"))?
            .read_to_end(&mut stdout)?;
        let mut stderr = Vec::new();
        self.child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("control command stderr was not piped"))?
            .read_to_end(&mut stderr)?;
        Ok(Output {
            status,
            stdout,
            stderr,
        })
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.reaped {
            let _killed = self.child.kill();
            let _status = self.child.wait();
        }
    }
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "immortalctl-routing-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(fs::canonicalize(path)?))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _removed = fs::remove_dir_all(&self.0);
    }
}
