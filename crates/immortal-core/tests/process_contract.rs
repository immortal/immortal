//! Single-threaded native contracts for direct fork-backed process mechanisms.
//!
//! The standard Rust test harness may create worker threads. This executable
//! deliberately has no harness so every `fork()` occurs in a process which has
//! never created another thread. It never creates a Tokio runtime.

use std::{
    error::Error,
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io,
    os::fd::{AsRawFd, OwnedFd},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use immortal_core::process::{
    ChildEvent, ProcessCommand, ProcessDescriptor, ProcessEnvironment, ProcessSignal, SignalTarget,
    SpawnError, SpawnFailure, SpawnStage, SpawnedProcess, reap_any_event, signal, spawn,
    spawn_with_descriptors,
};

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

fn main() -> Result<(), Box<dyn Error>> {
    exits_with_reported_status()?;
    exec_failure_is_not_reported_as_a_started_child()?;
    explicit_descriptor_allow_list_is_enforced()?;
    stop_continue_and_group_termination_are_observable()?;
    Ok(())
}

fn explicit_descriptor_allow_list_is_enforced() -> Result<(), Box<dyn Error>> {
    let files = DescriptorFiles::new()?;
    let inherited = writable_file(files.inherited())?;
    let inherited_descriptor = inherited.as_raw_fd();
    let omitted = writable_file(files.omitted())?;
    let omitted_descriptor = omitted.as_raw_fd();

    let mut environment = ProcessEnvironment::new();
    environment.insert(
        OsString::from("INHERITED_FD"),
        OsString::from(inherited_descriptor.to_string()),
    );
    environment.insert(
        OsString::from("OMITTED_FD"),
        OsString::from(omitted_descriptor.to_string()),
    );
    let mut command = ProcessCommand::new("/bin/sh");
    command
        .argument("-c")
        .argument(
            "if eval \"printf leaked >&$OMITTED_FD\" 2>/dev/null; then exit 91; fi; \
             eval \"printf preserved >&$INHERITED_FD\"",
        )
        .environment(environment);
    let descriptor = ProcessDescriptor::inherit(OwnedFd::from(inherited));
    let mut child = ChildGuard::new(spawn_with_descriptors(
        command,
        STARTUP_TIMEOUT,
        [descriptor],
    )?);
    let event = child.wait_for(ChildEvent::is_terminal, EVENT_TIMEOUT)?;
    if !matches!(event, ChildEvent::Exited { code: 0, .. }) {
        return Err(
            io::Error::other(format!("descriptor allow-list child failed: {event:?}")).into(),
        );
    }
    drop(omitted);
    if fs::read(files.inherited())? != b"preserved" {
        return Err(io::Error::other("explicitly inherited descriptor was not preserved").into());
    }
    if !fs::read(files.omitted())?.is_empty() {
        return Err(io::Error::other("omitted descriptor survived child execution").into());
    }
    Ok(())
}

fn writable_file(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(path)
}

fn exits_with_reported_status() -> Result<(), Box<dyn Error>> {
    let mut command = ProcessCommand::new("/bin/sh");
    command.argument("-c").argument("exit 7");
    let mut child = ChildGuard::new(spawn(command, STARTUP_TIMEOUT)?);
    let event = child.wait_for(ChildEvent::is_terminal, EVENT_TIMEOUT)?;
    if event
        != (ChildEvent::Exited {
            pid: child.process().process(),
            code: 7,
        })
    {
        return Err(io::Error::other(format!("expected exit status 7, received {event:?}")).into());
    }
    Ok(())
}

fn exec_failure_is_not_reported_as_a_started_child() -> Result<(), Box<dyn Error>> {
    let error = match spawn(
        ProcessCommand::new("/definitely/not/an/immortal-executable"),
        STARTUP_TIMEOUT,
    ) {
        Err(error) => error,
        Ok(child) => {
            let _guard = ChildGuard::new(child);
            return Err(io::Error::other("missing executable unexpectedly started").into());
        }
    };
    assert_spawn_failure(&error, SpawnStage::Execute, SpawnFailure::OperatingSystem)?;
    if error.cleanup_pending().is_some() {
        return Err(io::Error::other("exec failure left an unreaped child").into());
    }
    Ok(())
}

fn stop_continue_and_group_termination_are_observable() -> Result<(), Box<dyn Error>> {
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("5");
    let mut child = ChildGuard::new(spawn(command, STARTUP_TIMEOUT)?);

    signal(
        SignalTarget::Process(child.process().process()),
        ProcessSignal::Stop,
    )?;
    let stopped = child.wait_for(
        |event| matches!(event, ChildEvent::Stopped { .. }),
        EVENT_TIMEOUT,
    )?;
    if stopped.pid() != child.process().process() {
        return Err(io::Error::other("stop event identified another child").into());
    }

    signal(
        SignalTarget::Process(child.process().process()),
        ProcessSignal::Continue,
    )?;
    let continued = child.wait_for(
        |event| matches!(event, ChildEvent::Continued { .. }),
        EVENT_TIMEOUT,
    )?;
    if continued.pid() != child.process().process() {
        return Err(io::Error::other("continue event identified another child").into());
    }

    signal(
        SignalTarget::Group(child.process().group()),
        ProcessSignal::Terminate,
    )?;
    let terminated = child.wait_for(ChildEvent::is_terminal, EVENT_TIMEOUT)?;
    let ChildEvent::Signaled { pid, signal: raw } = terminated else {
        return Err(io::Error::other(format!(
            "expected signal termination, received {terminated:?}"
        ))
        .into());
    };
    if pid != child.process().process() || raw == 0 {
        return Err(io::Error::other("invalid signal termination event").into());
    }
    Ok(())
}

fn assert_spawn_failure(
    error: &SpawnError,
    stage: SpawnStage,
    failure: SpawnFailure,
) -> Result<(), Box<dyn Error>> {
    if error.stage() != stage || error.failure() != failure {
        return Err(io::Error::other(format!(
            "unexpected spawn failure: stage={:?} failure={:?}",
            error.stage(),
            error.failure()
        ))
        .into());
    }
    Ok(())
}

struct ChildGuard {
    process: SpawnedProcess,
    reaped: bool,
}

impl ChildGuard {
    const fn new(process: SpawnedProcess) -> Self {
        Self {
            process,
            reaped: false,
        }
    }

    const fn process(&self) -> SpawnedProcess {
        self.process
    }

    fn wait_for(
        &mut self,
        predicate: impl Fn(ChildEvent) -> bool,
        timeout: Duration,
    ) -> io::Result<ChildEvent> {
        let deadline = Instant::now() + timeout;
        loop {
            match reap_any_event() {
                Ok(Some(event)) => {
                    if event.pid() != self.process.process() {
                        return Err(io::Error::other(format!(
                            "reaped unowned child event {event:?}"
                        )));
                    }
                    if event.is_terminal() {
                        self.reaped = true;
                    }
                    if predicate(event) {
                        return Ok(event);
                    }
                    if self.reaped {
                        return Err(io::Error::other(
                            "child was reaped before the expected event was observed",
                        ));
                    }
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for child event",
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
        let _ = signal(
            SignalTarget::Group(self.process.group()),
            ProcessSignal::Kill,
        );
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while Instant::now() < deadline {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == self.process.process() && event.is_terminal() => {
                    self.reaped = true;
                    return;
                }
                Ok(Some(_) | None) => thread::sleep(POLL_INTERVAL),
                Err(_) => return,
            }
        }
    }
}

struct DescriptorFiles {
    inherited: PathBuf,
    omitted: PathBuf,
}

impl DescriptorFiles {
    fn new() -> io::Result<Self> {
        let directory = std::env::temp_dir();
        let process = std::process::id();
        let inherited = directory.join(format!("immortal-inherited-descriptor-{process}"));
        let omitted = directory.join(format!("immortal-omitted-descriptor-{process}"));
        remove_if_present(&inherited)?;
        remove_if_present(&omitted)?;
        Ok(Self { inherited, omitted })
    }

    fn inherited(&self) -> &Path {
        &self.inherited
    }

    fn omitted(&self) -> &Path {
        &self.omitted
    }
}

impl Drop for DescriptorFiles {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.inherited);
        let _ = fs::remove_file(&self.omitted);
    }
}

fn remove_if_present(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}
