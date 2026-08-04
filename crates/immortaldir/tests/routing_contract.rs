//! Black-box contract for directory-action startup and routing.

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
    let binary = Path::new(env!("CARGO_BIN_EXE_immortaldir"));
    let root = TestDirectory::new()?;
    let definitions = root.path().join("definitions");
    let runtime = root.path().join("runtime");
    fs::create_dir(&definitions)?;
    fs::set_permissions(&definitions, fs::Permissions::from_mode(0o755))?;
    fs::create_dir(&runtime)?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    fs::write(
        definitions.join("api.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;

    let output = run(
        binary,
        &[
            OsStr::new("--runtime-dir"),
            runtime.as_os_str(),
            OsStr::new("--once"),
            OsStr::new("--dry-run"),
            definitions.as_os_str(),
        ],
    )?;
    require_status(output.status, ExitClass::Success, "reconcile route")?;
    if output.stdout != b"START\tapi\n" {
        return Err(format!(
            "unexpected reconciliation plan: {:?}",
            String::from_utf8_lossy(&output.stdout)
        )
        .into());
    }

    let invalid = run(binary, &[])?;
    require_status(invalid.status, ExitClass::Usage, "missing directory")
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
                    "immortaldir routing contract exceeded its deadline",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        };

        let mut stdout = Vec::new();
        self.child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("directory command stdout was not piped"))?
            .read_to_end(&mut stdout)?;
        let mut stderr = Vec::new();
        self.child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("directory command stderr was not piped"))?
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
            "immortaldir-routing-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
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
