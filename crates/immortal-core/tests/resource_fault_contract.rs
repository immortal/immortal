//! Adversarial resource contracts for the process-broker boundary.
//!
//! One single-threaded parent owns the broker, its client, and every deadline.
//! A bounded stop/continue storm proves that request and child-event routing
//! remains live without accumulating per-signal state. Descriptor exhaustion is
//! isolated in a re-executed subprocess whose reduced file limit cannot affect
//! the test runner; broker startup must return the operating-system error
//! without creating an unreaped child, and the control accept loop must absorb
//! the same exhaustion and still serve a request once descriptors return.

#[path = "support/broker_guard.rs"]
mod broker_guard;

use std::{
    env,
    error::Error,
    ffi::OsStr,
    fs::{self, File},
    io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    control::{
        ControlListener, GenerationMatch, Operation, Request, Response, ResponseCode, SignalScope,
        read_response, run_control_server, write_request,
    },
    process::{
        BrokerSignalScope, ChildEvent, ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand,
        ProcessSignal, reap_any_event, start_process_broker,
    },
    status::StatusSnapshot,
    supervisor::{Generation, StateMachine},
};
use tokio::{
    net::UnixStream,
    runtime::Builder,
    sync::{mpsc, watch},
    time::sleep,
};

use crate::broker_guard::BrokerGuard;

const CHILD_MODE: &str = "IMMORTAL_DESCRIPTOR_EXHAUSTION_CHILD";
const CONTROL_CHILD_MODE: &str = "IMMORTAL_CONTROL_EXHAUSTION_CHILD";
// Three times the accept loop's exhaustion backoff, so the loop is observed
// retrying rather than merely surviving one failed accept.
const ACCEPT_EXHAUSTION_PROBE: Duration = Duration::from_millis(350);
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
// Some kernels surface stopped and continued state through the broker's
// periodic reap sweep rather than one notification per transition.
const STORM_CYCLES: u16 = 32;
const STORM_TIMEOUT: Duration = Duration::from_secs(30);
const SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(5);

fn main() -> Result<(), Box<dyn Error>> {
    if env::var_os(CHILD_MODE).is_some() {
        return descriptor_exhaustion_child();
    }
    if env::var_os(CONTROL_CHILD_MODE).is_some() {
        return control_accept_exhaustion_child();
    }
    signal_storm_is_bounded()?;
    descriptor_exhaustion_is_bounded()?;
    control_accept_survives_descriptor_exhaustion()?;
    Ok(())
}

fn signal_storm_is_bounded() -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        tokio::time::timeout(STORM_TIMEOUT, run_signal_storm(endpoint.connect()?))
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    "process broker did not drain the bounded signal storm",
                )
            })??;
        Ok::<(), Box<dyn Error>>(())
    })?;
    drop(runtime);
    broker.wait()?;
    Ok(())
}

async fn run_signal_storm(mut client: ProcessBrokerClient) -> Result<(), Box<dyn Error>> {
    require_event(&mut client, |event| {
        matches!(event, ProcessBrokerEvent::Ready)
    })
    .await?;
    let generation = Generation::FIRST;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("30");
    client.spawn(generation, command, STARTUP_TIMEOUT).await?;
    require_event(&mut client, |event| {
        matches!(
            event,
            ProcessBrokerEvent::Started {
                generation: observed,
                ..
            } if *observed == generation
        )
    })
    .await?;

    for _ in 0..STORM_CYCLES {
        deliver_and_observe(&mut client, generation, ProcessSignal::Stop, |event| {
            matches!(event, ChildEvent::Stopped { .. })
        })
        .await?;
        deliver_and_observe(&mut client, generation, ProcessSignal::Continue, |event| {
            matches!(event, ChildEvent::Continued { .. })
        })
        .await?;
    }

    deliver_and_observe(
        &mut client,
        generation,
        ProcessSignal::Kill,
        ChildEvent::is_terminal,
    )
    .await?;
    client.shutdown().await?;
    require_event(&mut client, |event| {
        matches!(event, ProcessBrokerEvent::ShutdownComplete)
    })
    .await
}

async fn deliver_and_observe(
    client: &mut ProcessBrokerClient,
    generation: Generation,
    signal: ProcessSignal,
    child_matches: impl FnOnce(ChildEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    client
        .signal(generation, BrokerSignalScope::Group, signal)
        .await?;
    require_event(client, |event| {
        matches!(
            event,
            ProcessBrokerEvent::SignalDelivered {
                generation: observed
            } if *observed == generation
        )
    })
    .await?;
    require_event(client, |event| {
        matches!(
            event,
            ProcessBrokerEvent::Child {
                generation: observed,
                event,
            } if *observed == generation && child_matches(*event)
        )
    })
    .await
}

async fn require_event(
    client: &mut ProcessBrokerClient,
    predicate: impl FnOnce(&ProcessBrokerEvent) -> bool,
) -> Result<(), Box<dyn Error>> {
    let event = client.next_event().await?;
    if predicate(&event) {
        Ok(())
    } else {
        Err(io::Error::other(format!("unexpected process-broker event: {event:?}")).into())
    }
}

fn descriptor_exhaustion_is_bounded() -> Result<(), Box<dyn Error>> {
    let executable = env::current_exe()?;
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg("set -e; ulimit -n 32; exec \"$1\"")
        .arg("immortal-resource-fault")
        .arg(executable)
        .env(CHILD_MODE, OsStr::new("1"))
        .spawn()?;
    let status = wait_for_subprocess(child, SUBPROCESS_TIMEOUT)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "descriptor-exhaustion subprocess failed with {status}"
        ))
        .into())
    }
}

fn wait_for_subprocess(mut child: Child, timeout: Duration) -> io::Result<ExitStatus> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "descriptor-exhaustion subprocess exceeded its deadline",
            ));
        }
        thread::sleep(POLL_INTERVAL);
    }
}

fn descriptor_exhaustion_child() -> Result<(), Box<dyn Error>> {
    let mut descriptors = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(descriptor) => descriptors.push(descriptor),
            Err(error) if is_descriptor_exhaustion(&error) => break,
            Err(error) => return Err(error.into()),
        }
        if descriptors.len() > 64 {
            return Err(io::Error::other(
                "descriptor limit was not reduced for the exhaustion subprocess",
            )
            .into());
        }
    }

    let error = start_process_broker()
        .err()
        .ok_or_else(|| io::Error::other("broker started with no descriptor capacity"))?;
    if !is_descriptor_exhaustion(&error) {
        return Err(io::Error::other(format!(
            "broker returned an unexpected exhaustion error: {error}"
        ))
        .into());
    }
    match reap_any_event() {
        Err(error) if error.raw_os_error() == Some(libc::ECHILD) => Ok(()),
        Ok(Some(event)) => Err(io::Error::other(format!(
            "descriptor exhaustion left a waitable broker child: {event:?}"
        ))
        .into()),
        Ok(None) => Err(io::Error::other("descriptor exhaustion left a live broker child").into()),
        Err(error) => Err(error.into()),
    }
}

fn is_descriptor_exhaustion(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(libc::EMFILE | libc::ENFILE))
}

/// Regression: exhausted descriptors must not end the control accept loop.
///
/// `accept` fails with `EMFILE` whenever a connection is pending and the
/// process holds no free descriptor. Treating every listener `Io` failure as
/// fatal ended `run_control_server`, closing its command channel and stopping
/// the supervisor, which then let the broker kill a healthy service. The
/// reduced limit is isolated in a re-executed subprocess so it cannot affect
/// the test runner.
fn control_accept_survives_descriptor_exhaustion() -> Result<(), Box<dyn Error>> {
    let executable = env::current_exe()?;
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg("set -e; ulimit -n 32; exec \"$1\"")
        .arg("immortal-control-fault")
        .arg(executable)
        .env(CONTROL_CHILD_MODE, OsStr::new("1"))
        .spawn()?;
    let status = wait_for_subprocess(child, SUBPROCESS_TIMEOUT)?;
    if status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "control accept-exhaustion subprocess failed with {status}"
        ))
        .into())
    }
}

/// Owns the subprocess runtime directory so a failure still removes it.
struct RuntimeDirectory(PathBuf);

impl RuntimeDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let path = Path::new("/tmp").join(format!("immortal-accept-{}", std::process::id()));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self(fs::canonicalize(path)?))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn control_accept_exhaustion_child() -> Result<(), Box<dyn Error>> {
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(serve_under_descriptor_exhaustion())
}

async fn serve_under_descriptor_exhaustion() -> Result<(), Box<dyn Error>> {
    let directory = RuntimeDirectory::new()?;
    let path = directory.path().join("control.sock");
    let listener = Arc::new(ControlListener::bind(&path, 2)?);
    let (sender, mut commands) = mpsc::channel(1);
    let (shutdown_sender, shutdown) = watch::channel(false);

    // Connect before exhausting so the pending connection, the request write,
    // and the response read all need no additional descriptor. Only the
    // server's accept does.
    let mut client = UnixStream::connect(&path).await?;
    let descriptors = exhaust_descriptors()?;

    let server = tokio::spawn(run_control_server(Arc::clone(&listener), sender, shutdown));
    sleep(ACCEPT_EXHAUSTION_PROBE).await;
    if server.is_finished() {
        return Err(io::Error::other("control server ended on descriptor exhaustion").into());
    }
    drop(descriptors);

    let request = Request {
        operation: Operation::Status,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    let exchange = async {
        write_request(&mut client, &request).await?;
        let response = read_response(&mut client).await?;
        Ok::<Response, Box<dyn Error>>(response)
    };
    let dispatch = async {
        let command = commands
            .recv()
            .await
            .ok_or_else(|| io::Error::other("control server stopped before recovering"))?;
        command
            .respond(Response {
                code: ResponseCode::Ok,
                generation: None,
                message: "recovered".to_owned(),
                status: Some(StatusSnapshot::from_machine(&StateMachine::default())),
            })
            .map_err(|_| io::Error::other("client disconnected before the response"))?;
        Ok::<(), Box<dyn Error>>(())
    };
    let (exchange_result, dispatch_result) = tokio::join!(exchange, dispatch);
    dispatch_result?;
    if exchange_result?.message != "recovered" {
        return Err(io::Error::other("control server answered with an unexpected response").into());
    }

    shutdown_sender.send(true)?;
    server.await??;
    Ok(())
}

/// Hold every remaining descriptor so the next `accept` cannot allocate one.
fn exhaust_descriptors() -> Result<Vec<File>, Box<dyn Error>> {
    let mut descriptors = Vec::new();
    loop {
        match File::open("/dev/null") {
            Ok(descriptor) => descriptors.push(descriptor),
            Err(error) if is_descriptor_exhaustion(&error) => return Ok(descriptors),
            Err(error) => return Err(error.into()),
        }
        if descriptors.len() > 64 {
            return Err(io::Error::other(
                "descriptor limit was not reduced for the exhaustion subprocess",
            )
            .into());
        }
    }
}
