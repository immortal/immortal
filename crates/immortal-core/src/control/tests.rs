//! Facade-spanning control protocol, decision, transport, and server tests.

use std::{error::Error, time::Duration};

use tokio::io::{AsyncWriteExt, duplex};

#[cfg(unix)]
use std::{
    fs, io,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};
#[cfg(unix)]
use tokio::net::UnixStream;
#[cfg(unix)]
use tokio::sync::{mpsc, watch};

#[cfg(unix)]
use super::{
    AcceptError, ControlListener, accept_error_is_exhaustion, accept_error_is_transient,
    peer_is_authorized, run_control_server,
};
use super::{
    ControlEffect, GenerationMatch, MAX_FRAME_BYTES, Operation, PROTOCOL_VERSION, ProtocolError,
    Request, Response, ResponseCode, Signal, SignalScope, StopCompletion, TransportError,
    decide_request, read_request, read_request_with_timeout, read_response, write_request,
    write_response,
};
use crate::status::{
    LastResult, LoggerStatus, MAX_STATUS_ARGUMENTS, ReadinessStatus, ServiceState, StatusSnapshot,
};
use crate::supervisor::{
    DesiredState, FailureReason, Generation, RestartDecision, StateMachine, SupervisorState,
};

#[cfg(unix)]
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[cfg(unix)]
struct TestDirectory(PathBuf);

#[cfg(unix)]
impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new("/tmp").join(format!(
            "immortal-control-{}-{sequence}",
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

#[cfg(unix)]
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn all_signal_names_round_trip() -> Result<(), Box<dyn Error>> {
    for name in [
        "usr1", "usr2", "alrm", "cont", "hup", "int", "kill", "ttin", "ttou", "quit", "stop",
        "term", "winch",
    ] {
        let signal = Signal::from_name(name).ok_or(ProtocolError::UnknownSignal(u8::MAX))?;
        let request = Request {
            operation: Operation::Signal,
            service: "api.worker-1".to_owned(),
            expected_generation: GenerationMatch::Exact(Generation::from_protocol(42)),
            scope: SignalScope::Group,
            signal: Some(signal),
        };
        assert_eq!(Request::decode(&request.encode()?)?, request);
        assert_eq!(signal.name(), name);
    }
    Ok(())
}

#[test]
fn status_is_valid_before_the_first_child() {
    let mut machine = StateMachine::default();
    let decision = decide_request(
        "api",
        &mut machine,
        &Request {
            operation: Operation::Status,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        },
    );
    assert_eq!(decision.response.code, ResponseCode::Ok);
    assert_eq!(decision.response.generation, None);
    assert_eq!(decision.response.message, "desired=up state=down");
    assert_eq!(decision.effect, ControlEffect::None);
}

#[test]
fn restart_is_bound_to_the_exact_live_generation() -> Result<(), Box<dyn Error>> {
    let mut machine = ready_machine()?;
    let generation = machine
        .state()
        .live_generation()
        .ok_or("ready machine has no generation")?;
    let stale = decide_request(
        "api",
        &mut machine,
        &Request {
            operation: Operation::Restart,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        },
    );
    assert_eq!(stale.response.code, ResponseCode::Conflict);
    assert_eq!(stale.effect, ControlEffect::None);

    let accepted = decide_request(
        "api",
        &mut machine,
        &Request {
            operation: Operation::Restart,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: None,
        },
    );
    assert_eq!(accepted.response.code, ResponseCode::Ok);
    assert_eq!(machine.desired(), DesiredState::Up);
    assert_eq!(
        accepted.effect,
        ControlEffect::StopGroup {
            generation,
            after: StopCompletion::Restart,
        }
    );
    Ok(())
}

#[test]
fn signal_requires_a_live_generation() {
    let mut machine = StateMachine::default();
    let decision = decide_request(
        "api",
        &mut machine,
        &Request {
            operation: Operation::Signal,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: Some(Signal::Hangup),
        },
    );
    assert_eq!(decision.response.code, ResponseCode::Invalid);
    assert_eq!(decision.effect, ControlEffect::None);
}

#[test]
fn manual_restart_resets_configured_failure() -> Result<(), Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.begin_start()?;
    let generation = machine.preconditions_ready()?;
    machine.child_started(generation)?;
    machine.child_reaped(generation, RestartDecision::Fail(FailureReason::RetryLimit))?;
    assert!(matches!(machine.state(), SupervisorState::Failed(_)));

    let decision = decide_request(
        "api",
        &mut machine,
        &Request {
            operation: Operation::Restart,
            service: "api".to_owned(),
            expected_generation: GenerationMatch::NoChild,
            scope: SignalScope::Main,
            signal: None,
        },
    );
    assert_eq!(machine.state(), SupervisorState::Down);
    assert_eq!(machine.desired(), DesiredState::Up);
    assert_eq!(decision.effect, ControlEffect::BeginStart);
    Ok(())
}

fn ready_machine() -> Result<StateMachine, Box<dyn Error>> {
    let mut machine = StateMachine::default();
    machine.begin_start()?;
    let generation = machine.preconditions_ready()?;
    machine.child_started(generation)?;
    machine.child_ready(generation)?;
    Ok(machine)
}

#[test]
fn every_lifecycle_operation_round_trips() -> Result<(), Box<dyn Error>> {
    for operation in [
        Operation::Status,
        Operation::Start,
        Operation::Stop,
        Operation::Restart,
        Operation::Once,
        Operation::Exit,
        Operation::Halt,
    ] {
        let request = Request {
            operation,
            service: "api".to_owned(),
            expected_generation: if operation == Operation::Status {
                GenerationMatch::Any
            } else {
                GenerationMatch::NoChild
            },
            scope: SignalScope::Main,
            signal: None,
        };
        assert_eq!(Request::decode(&request.encode()?)?, request);
    }
    Ok(())
}

#[test]
fn rejects_oversized_truncated_and_trailing_frames() -> Result<(), Box<dyn Error>> {
    assert!(matches!(
        Request::decode(&vec![0; MAX_FRAME_BYTES + 1]),
        Err(ProtocolError::FrameTooLarge(_))
    ));
    assert!(matches!(
        Request::decode(b"IMMO"),
        Err(ProtocolError::Truncated)
    ));
    let mut valid = Request {
        operation: Operation::Status,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::NoChild,
        scope: SignalScope::Main,
        signal: None,
    }
    .encode()?;
    valid.push(0);
    assert!(matches!(
        Request::decode(&valid),
        Err(ProtocolError::TrailingBytes)
    ));
    Ok(())
}

#[test]
fn rejects_unknown_version_operation_scope_and_signal() -> Result<(), Box<dyn Error>> {
    let request = Request {
        operation: Operation::Signal,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::NoChild,
        scope: SignalScope::Main,
        signal: Some(Signal::Hangup),
    };
    let frame = request.encode()?;

    let mut version = frame.clone();
    if let Some(byte) = version.get_mut(4) {
        *byte = PROTOCOL_VERSION.saturating_add(1);
    }
    assert!(matches!(
        Request::decode(&version),
        Err(ProtocolError::UnsupportedVersion(_))
    ));

    let mut operation = frame.clone();
    if let Some(byte) = operation.get_mut(5) {
        *byte = u8::MAX;
    }
    assert!(matches!(
        Request::decode(&operation),
        Err(ProtocolError::UnknownOperation(_))
    ));

    let mut scope = frame.clone();
    if let Some(byte) = scope.get_mut(6) {
        *byte = u8::MAX;
    }
    assert!(matches!(
        Request::decode(&scope),
        Err(ProtocolError::UnknownScope(_))
    ));

    let mut signal = frame;
    if let Some(byte) = signal.get_mut(7) {
        *byte = u8::MAX;
    }
    assert!(matches!(
        Request::decode(&signal),
        Err(ProtocolError::UnknownSignal(_))
    ));
    Ok(())
}

#[test]
fn rejects_unsafe_names_and_inconsistent_signal_fields() {
    let unsafe_name = Request {
        operation: Operation::Status,
        service: "../api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    assert!(matches!(
        unsafe_name.encode(),
        Err(ProtocolError::UnsafeServiceName)
    ));

    let missing = Request {
        operation: Operation::Signal,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::NoChild,
        scope: SignalScope::Main,
        signal: None,
    };
    assert!(matches!(
        missing.encode(),
        Err(ProtocolError::MissingSignal)
    ));

    let unguarded = Request {
        operation: Operation::Restart,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    assert!(matches!(
        unguarded.encode(),
        Err(ProtocolError::MissingGenerationMatch)
    ));
}

#[test]
fn bounded_response_round_trips() -> Result<(), Box<dyn Error>> {
    for code in [
        ResponseCode::Ok,
        ResponseCode::NotFound,
        ResponseCode::PermissionDenied,
        ResponseCode::Conflict,
        ResponseCode::Invalid,
        ResponseCode::Internal,
    ] {
        let response = Response {
            code,
            generation: Some(Generation::from_protocol(7)),
            message: "state=ready".to_owned(),
            status: None,
        };
        assert_eq!(Response::decode(&response.encode()?)?, response);
    }
    Ok(())
}

#[test]
fn typed_status_payload_round_trips_every_field() -> Result<(), Box<dyn Error>> {
    let status = StatusSnapshot {
        supervisor_pid: Some(101),
        main_pid: Some(202),
        desired: DesiredState::Up,
        state: ServiceState::Backoff,
        readiness: ReadinessStatus::TimedOut,
        uptime_seconds: Some(17),
        down_seconds: Some(3),
        starts: 9,
        failures: 4,
        last_result: Some(LastResult::Signaled(9)),
        backoff_seconds: Some(8),
        logger: LoggerStatus::Failed,
        command: vec!["/usr/bin/api".to_owned(), "argument with spaces".to_owned()],
    };
    let response = Response {
        code: ResponseCode::Ok,
        generation: Some(Generation::from_protocol(7)),
        message: "status".to_owned(),
        status: Some(status),
    };
    assert_eq!(Response::decode(&response.encode()?)?, response);
    Ok(())
}

#[test]
fn typed_status_rejects_unknown_payload_and_unbounded_arguments() -> Result<(), Box<dyn Error>> {
    let response = Response {
        code: ResponseCode::Ok,
        generation: None,
        message: String::new(),
        status: None,
    };
    let mut unknown = response.encode()?;
    if let Some(payload) = unknown.get_mut(6) {
        *payload = u8::MAX;
    }
    assert!(matches!(
        Response::decode(&unknown),
        Err(ProtocolError::UnknownResponsePayload(_))
    ));

    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.command = vec!["argument".to_owned(); MAX_STATUS_ARGUMENTS + 1];
    assert!(matches!(
        (Response {
            code: ResponseCode::Ok,
            generation: None,
            message: String::new(),
            status: Some(status),
        })
        .encode(),
        Err(ProtocolError::TooManyStatusArguments)
    ));
    Ok(())
}

#[test]
fn response_rejects_oversized_and_malformed_data() {
    let oversized = Response {
        code: ResponseCode::Ok,
        generation: None,
        message: "x".repeat(MAX_FRAME_BYTES),
        status: None,
    };
    assert!(matches!(
        oversized.encode(),
        Err(ProtocolError::MessageTooLong | ProtocolError::FrameTooLarge(_))
    ));
    assert!(matches!(
        Response::decode(b"IMMO"),
        Err(ProtocolError::Truncated)
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn async_transport_round_trips_one_request_and_response() -> Result<(), Box<dyn Error>> {
    let request = Request {
        operation: Operation::Signal,
        service: "worker".to_owned(),
        expected_generation: GenerationMatch::Exact(Generation::from_protocol(9)),
        scope: SignalScope::Group,
        signal: Some(Signal::Terminate),
    };
    let response = Response {
        code: ResponseCode::Ok,
        generation: Some(Generation::from_protocol(9)),
        message: "signal accepted".to_owned(),
        status: None,
    };
    let (mut client, mut server) = duplex(1024);

    write_request(&mut client, &request).await?;
    assert_eq!(read_request(&mut server).await?, request);
    write_response(&mut server, &response).await?;
    assert_eq!(read_response(&mut client).await?, response);
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_transport_times_out_idle_clients() {
    let (_writer, mut reader) = duplex(64);
    let result = read_request_with_timeout(&mut reader, Duration::from_millis(1)).await;
    assert!(matches!(result, Err(TransportError::Timeout)));
}

#[tokio::test(flavor = "current_thread")]
async fn async_transport_rejects_declared_oversized_frames_before_payload_read()
-> Result<(), Box<dyn Error>> {
    let request = Request {
        operation: Operation::Status,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    let mut header = request.encode()?;
    header.truncate(super::HEADER_BYTES);
    let length = u16::MAX.to_be_bytes();
    if let Some(target) = header.get_mut(super::HEADER_BYTES - 2..) {
        target.copy_from_slice(&length);
    }
    let (mut writer, mut reader) = duplex(64);
    writer.write_all(&header).await?;

    let result = read_request(&mut reader).await;
    assert!(matches!(
        result,
        Err(TransportError::Protocol(ProtocolError::FrameTooLarge(_)))
    ));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn async_transport_reports_truncated_frames() -> Result<(), Box<dyn Error>> {
    let (mut writer, mut reader) = duplex(64);
    writer.write_all(b"IMMO").await?;
    writer.shutdown().await?;

    let result = read_request(&mut reader).await;
    assert!(matches!(result, Err(TransportError::Io(_))));
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn owned_listener_restricts_mode_and_authenticates_owner() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("control.sock");
    let listener = ControlListener::bind(&path, 2)?;
    let mut client = UnixStream::connect(&path).await?;
    let mut connection = listener.accept().await?;

    assert_eq!(connection.peer().uid, listener.owner_uid());
    assert_eq!(fs::symlink_metadata(&path)?.mode() & 0o777, 0o600);
    let request = Request {
        operation: Operation::Status,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    write_request(&mut client, &request).await?;
    assert_eq!(read_request(connection.stream_mut()).await?, request);

    drop(connection);
    drop(client);
    drop(listener);
    assert!(!path.exists());
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn listener_bounds_active_clients() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("control.sock");
    let listener = ControlListener::bind(&path, 1)?;
    let first_client = UnixStream::connect(&path).await?;
    let first = listener.accept().await?;
    let second_client = UnixStream::connect(&path).await?;

    assert!(matches!(
        listener.accept_with_timeout(Duration::from_millis(1)).await,
        Err(AcceptError::Timeout)
    ));
    drop(first);
    let second = listener.accept().await?;

    drop(second);
    drop(second_client);
    drop(first_client);
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn server_loop_isolates_bad_clients_and_dispatches_authenticated_requests()
-> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("immortal.sock");
    let listener = Arc::new(ControlListener::bind(&path, 2)?);
    let owner_uid = listener.owner_uid();
    let (sender, mut commands) = mpsc::channel(1);
    let (shutdown_sender, shutdown) = watch::channel(false);
    let mut server = tokio::spawn(run_control_server(Arc::clone(&listener), sender, shutdown));

    let mut malformed = UnixStream::connect(&path).await?;
    malformed.write_all(b"bad").await?;
    drop(malformed);

    let request = Request {
        operation: Operation::Status,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    let client = async {
        let mut stream = UnixStream::connect(&path).await?;
        write_request(&mut stream, &request).await?;
        let response = read_response(&mut stream).await?;
        Ok::<Response, Box<dyn Error>>(response)
    };
    let dispatch = async {
        let Some(command) = commands.recv().await else {
            let result = (&mut server).await;
            return Err(format!("control server stopped before dispatch: {result:?}").into());
        };
        assert_eq!(command.request(), &request);
        assert!(command.peer().uid == 0 || command.peer().uid == owner_uid);
        command
            .respond(Response {
                code: ResponseCode::Ok,
                generation: None,
                message: "dispatched".to_owned(),
                status: Some(StatusSnapshot::from_machine(&StateMachine::default())),
            })
            .map_err(|_| "client disconnected before response")?;
        Ok::<(), Box<dyn Error>>(())
    };
    let (client_result, dispatch_result) = tokio::join!(client, dispatch);
    dispatch_result?;
    assert_eq!(client_result?.message, "dispatched");

    shutdown_sender.send(true)?;
    server.await??;
    Ok(())
}

#[cfg(unix)]
#[test]
fn listener_refuses_unsafe_or_existing_paths() -> Result<(), Box<dyn Error>> {
    assert!(ControlListener::bind(Path::new("relative.sock"), 1).is_err());
    let directory = TestDirectory::new()?;
    let path = directory.path().join("control.sock");
    fs::write(&path, b"do not replace")?;
    assert!(ControlListener::bind(&path, 1).is_err());
    assert_eq!(fs::read(&path)?, b"do not replace");
    Ok(())
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn listener_cleanup_never_removes_a_replacement() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("control.sock");
    let listener = ControlListener::bind(&path, 1)?;
    fs::remove_file(&path)?;
    fs::write(&path, b"replacement")?;

    drop(listener);
    assert_eq!(fs::read(&path)?, b"replacement");
    Ok(())
}

#[cfg(unix)]
#[test]
fn peer_policy_allows_only_root_or_owner() {
    assert!(peer_is_authorized(0, 1000));
    assert!(peer_is_authorized(1000, 1000));
    assert!(!peer_is_authorized(1001, 1000));
}

#[cfg(unix)]
#[test]
fn accept_classification_separates_peer_and_resource_faults_from_listener_faults() {
    for kind in [
        io::ErrorKind::ConnectionAborted,
        io::ErrorKind::ConnectionReset,
        io::ErrorKind::Interrupted,
        io::ErrorKind::WouldBlock,
    ] {
        let error = io::Error::new(kind, "peer or scheduling fault");
        assert!(
            accept_error_is_transient(&error),
            "{kind:?} must be transient"
        );
        assert!(
            !accept_error_is_exhaustion(&error),
            "{kind:?} must not be paced as exhaustion"
        );
    }

    for errno in [libc::EMFILE, libc::ENFILE, libc::ENOBUFS, libc::ENOMEM] {
        let error = io::Error::from_raw_os_error(errno);
        assert!(
            accept_error_is_transient(&error),
            "errno {errno} must be transient"
        );
        assert!(
            accept_error_is_exhaustion(&error),
            "errno {errno} must be paced"
        );
    }

    for errno in [libc::EBADF, libc::EINVAL, libc::ENOTSOCK, libc::EFAULT] {
        let error = io::Error::from_raw_os_error(errno);
        assert!(
            !accept_error_is_transient(&error),
            "errno {errno} must stay fatal"
        );
    }
}

/// A burst of aborted peers must leave the control server usable.
///
/// A peer that disconnects before the server reads its credentials surfaces
/// either as a credential failure or, depending on the platform and timing, as
/// an aborted accept. Both are attributable to the peer, so neither may end
/// `run_control_server` and take the supervisor down with it.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn server_loop_survives_aborted_peers() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("immortal.sock");
    let listener = Arc::new(ControlListener::bind(&path, 2)?);
    let (sender, mut commands) = mpsc::channel(1);
    let (shutdown_sender, shutdown) = watch::channel(false);
    let mut server = tokio::spawn(run_control_server(Arc::clone(&listener), sender, shutdown));

    for _ in 0..32 {
        drop(UnixStream::connect(&path).await?);
        tokio::task::yield_now().await;
    }

    let request = Request {
        operation: Operation::Status,
        service: "api".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    let client = async {
        let mut stream = UnixStream::connect(&path).await?;
        write_request(&mut stream, &request).await?;
        let response = read_response(&mut stream).await?;
        Ok::<Response, Box<dyn Error>>(response)
    };
    let dispatch = async {
        // Aborted peers never reach dispatch, so the only command is the
        // legitimate request above.
        let Some(command) = commands.recv().await else {
            let result = (&mut server).await;
            return Err(format!("control server stopped after aborted peers: {result:?}").into());
        };
        command
            .respond(Response {
                code: ResponseCode::Ok,
                generation: None,
                message: "survived".to_owned(),
                status: Some(StatusSnapshot::from_machine(&StateMachine::default())),
            })
            .map_err(|_| "client disconnected before response")?;
        Ok::<(), Box<dyn Error>>(())
    };
    let (client_result, dispatch_result) = tokio::join!(client, dispatch);
    dispatch_result?;
    assert_eq!(client_result?.message, "survived");

    shutdown_sender.send(true)?;
    server.await??;
    Ok(())
}

/// Absorbing transient accept failures must not remove the loop's exit paths.
#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
async fn server_loop_still_stops_when_the_supervisor_is_gone() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("immortal.sock");
    let listener = Arc::new(ControlListener::bind(&path, 1)?);
    let (sender, commands) = mpsc::channel(1);
    let (_shutdown_sender, shutdown) = watch::channel(false);
    let server = tokio::spawn(run_control_server(Arc::clone(&listener), sender, shutdown));

    drop(commands);
    match server.await? {
        Err(AcceptError::ShuttingDown) => Ok(()),
        other => Err(format!("orphaned control server did not stop: {other:?}").into()),
    }
}
