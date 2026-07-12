//! Black-box contracts for one authenticated, runtime-owned foreground supervisor.

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
    control::{
        GenerationMatch, Operation, Request, Response, ResponseCode, Signal, SignalScope,
        read_response, write_request,
    },
    exit::ExitClass,
    process::{ProcessGroupId, ProcessSignal, SignalTarget, signal as deliver_signal},
    status::ServiceState,
    supervisor::Generation,
};
use tokio::{net::UnixStream, runtime::Builder};

const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const POLL_INTERVAL: Duration = Duration::from_millis(20);

fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
    prove_controlled_lifecycle(binary)?;
    prove_explicit_exit_leaves_the_child(binary)
}

#[allow(clippy::too_many_lines)]
fn prove_controlled_lifecycle(binary: &Path) -> Result<(), Box<dyn Error>> {
    let runtime = RuntimeDirectory::new()?;
    let marker = runtime.root().join("events");
    let config = ConfigFile::new(
        "controlled",
        &format!(
            "version: 2\ncommand: [/bin/sh, -c, 'trap \"printf usr1\\n >> \\\"$MARKER\\\"\" USR1; trap \"exit 0\" TERM; printf start\\n >> \"$MARKER\"; while :; do sleep 1; done']\nenvironment:\n  MARKER: '{}'\n",
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

fn spawn_immortal(
    binary: &Path,
    config: &ConfigFile,
    service: &Path,
) -> Result<Child, Box<dyn Error>> {
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
        .stderr(Stdio::null())
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
        let root = std::env::temp_dir().join(format!(
            "immortal-controlled-{service_name}-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        let service = root.join(service_name);
        let socket = service.join("immortal.sock");
        Ok(Self {
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
        Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "control socket was not created",
        ))
    }
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
