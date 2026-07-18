//! Process, runtime-directory, and configuration fixtures for the controlled contract.
//!
//! Each fixture is an RAII guard that provisions temporary runtime state under
//! `/tmp` and tears it down on drop, terminating any surviving child, so a
//! scenario failing part-way through never leaks processes or directories.

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

use immortal_core::process::{ProcessId, ProcessSignal, SignalTarget, signal as deliver_signal};

use crate::{POLL_INTERVAL, path_str};

const DIAGNOSTIC_LIMIT: u64 = 16 * 1024;
static RUNTIME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn spawn_immortal(
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

pub(crate) struct RuntimeDirectory {
    diagnostics: PathBuf,
    name: String,
    root: PathBuf,
    service: PathBuf,
    socket: PathBuf,
}

impl RuntimeDirectory {
    pub(crate) fn new() -> std::io::Result<Self> {
        Self::new_named("api")
    }

    pub(crate) fn new_named(service_name: &str) -> std::io::Result<Self> {
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

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn service(&self) -> &Path {
        &self.service
    }

    pub(crate) fn service_name(&self) -> &str {
        &self.name
    }

    pub(crate) fn socket(&self) -> &Path {
        &self.socket
    }

    pub(crate) fn wait_for_socket(&self, timeout: Duration) -> std::io::Result<()> {
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

pub(crate) struct ConfigFile(PathBuf);

impl ConfigFile {
    pub(crate) fn new(name: &str, contents: &str) -> std::io::Result<Self> {
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

pub(crate) struct ChildGuard {
    child: Child,
    reaped: bool,
}

impl ChildGuard {
    pub(crate) const fn new(child: Child) -> Self {
        Self {
            child,
            reaped: false,
        }
    }

    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    pub(crate) fn wait(mut self, timeout: Duration) -> std::io::Result<ExitStatus> {
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

pub(crate) struct DetachedProcessGuard {
    killed: bool,
    process: ProcessId,
}

impl DetachedProcessGuard {
    pub(crate) const fn new(process: ProcessId) -> Self {
        Self {
            killed: false,
            process,
        }
    }

    pub(crate) fn kill(&mut self) {
        let _ = deliver_signal(SignalTarget::Process(self.process), ProcessSignal::Kill);
        self.killed = true;
    }
}

impl Drop for DetachedProcessGuard {
    fn drop(&mut self) {
        if !self.killed {
            let _ = deliver_signal(SignalTarget::Process(self.process), ProcessSignal::Kill);
        }
    }
}
