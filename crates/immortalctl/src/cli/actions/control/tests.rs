//! Discovery, contact, lifecycle, and output-rendering tests for the control action.

use std::{
    error::Error,
    fs, io,
    os::unix::fs::PermissionsExt,
    os::unix::net::UnixListener as StdUnixListener,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::Duration,
};

use immortal_core::control::{
    CONTROL_IO_TIMEOUT, ControlListener, GenerationMatch, Operation, Response, ResponseCode,
    SignalScope, TransportError, read_request, write_response,
};
use immortal_core::exit::ExitClass;
use immortal_core::process::{SpawnFailure, SpawnStage};
use immortal_core::status::{
    LastResult, LoggerLastResult, LoggerStatus, ServiceState, SpawnDetail, StatusSnapshot,
};
use immortal_core::supervisor::{Generation, StateMachine};
use tokio::time::sleep;

use super::WaitOutcome;
use super::{
    ActionError, ControlAction, DiscoveryRoot, OutputFormat, OutputRecord, RuntimeDiscovery,
    RuntimeOrigin, RuntimeScope, ScopedService, Target, contact, discover_from_roots,
    discovery_roots_with, exchange, operation_outcome, record, render_output, reply_error,
    reply_timeout, select_targets,
};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new("/var/tmp").join(format!(
            "immortalctl-action-{}-{sequence}",
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
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

fn custom_service(name: &str, directory: &Path, socket: PathBuf, owner_uid: u32) -> ScopedService {
    ScopedService {
        origin: RuntimeOrigin::Custom,
        service: immortal_core::runtime::RuntimeService {
            name: name.to_owned(),
            directory: directory.to_owned(),
            socket,
            owner_uid,
        },
    }
}

fn bind_discovered_service(root: &Path, name: &str) -> Result<StdUnixListener, Box<dyn Error>> {
    let directory = root.join(name);
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let socket = directory.join("immortal.sock");
    let listener = StdUnixListener::bind(&socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

#[test]
fn automatic_roots_merge_scopes_and_reject_ambiguous_names() -> Result<(), Box<dyn Error>> {
    let system = TestDirectory::new()?;
    let user = TestDirectory::new()?;
    let _system_listener = bind_discovered_service(system.path(), "api")?;
    let _user_listener = bind_discovered_service(user.path(), "api")?;
    let roots = vec![
        DiscoveryRoot {
            origin: RuntimeOrigin::System,
            path: system.path().to_owned(),
            user_owned: false,
            optional: true,
        },
        DiscoveryRoot {
            origin: RuntimeOrigin::User,
            path: user.path().to_owned(),
            user_owned: true,
            optional: true,
        },
    ];
    let mut diagnostics = Vec::new();
    let (services, failed) = discover_from_roots(roots, &mut diagnostics)?;
    assert!(!failed);
    assert_eq!(diagnostics, Vec::<u8>::new());
    assert_eq!(services.len(), 2);
    assert!(matches!(
        select_targets(services, &Target::Service("api".to_owned()), false),
        Err(ActionError::AmbiguousService(service)) if service == "api"
    ));
    Ok(())
}

#[test]
fn automatic_roots_isolate_an_unsafe_peer_root() -> Result<(), Box<dyn Error>> {
    let system = TestDirectory::new()?;
    let user = TestDirectory::new()?;
    let _listener = bind_discovered_service(user.path(), "worker")?;
    fs::set_permissions(system.path(), fs::Permissions::from_mode(0o777))?;
    let roots = vec![
        DiscoveryRoot {
            origin: RuntimeOrigin::System,
            path: system.path().to_owned(),
            user_owned: false,
            optional: true,
        },
        DiscoveryRoot {
            origin: RuntimeOrigin::User,
            path: user.path().to_owned(),
            user_owned: true,
            optional: true,
        },
    ];
    let mut diagnostics = Vec::new();
    let (services, failed) = discover_from_roots(roots, &mut diagnostics)?;
    assert!(failed);
    assert_eq!(services.len(), 1);
    assert_eq!(
        services.first().map(|service| service.origin),
        Some(RuntimeOrigin::User)
    );
    assert_ne!(diagnostics, Vec::<u8>::new());
    Ok(())
}

#[test]
fn automatic_roots_isolate_user_path_resolution_failure() {
    let (roots, error) =
        discovery_roots_with(&RuntimeDiscovery::Automatic(RuntimeScope::All), || {
            Err(io::Error::new(io::ErrorKind::NotFound, "missing home"))
        });
    assert!(error.is_some());
    assert_eq!(roots.len(), 1);
    assert_eq!(
        roots.first().map(|root| root.origin),
        Some(RuntimeOrigin::System)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn contact_sends_typed_request_and_receives_status() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let socket = directory.path().join("immortal.sock");
    let listener = ControlListener::bind(&socket, 1)?;
    let service = custom_service("api", directory.path(), socket, listener.owner_uid());
    let action = ControlAction {
        discovery: RuntimeDiscovery::Custom(directory.path().to_owned()),
        output: OutputFormat::Table,
        no_header: false,
        wait_timeout: Duration::from_secs(1),
        no_wait: false,
        target: Target::Service("api".to_owned()),
        scope: SignalScope::Main,
        signal: None,
    };

    let server = async {
        let mut connection = listener.accept().await?;
        let request = read_request(connection.stream_mut()).await?;
        assert_eq!(request.operation, Operation::Status);
        assert_eq!(request.service, "api");
        write_response(
            connection.stream_mut(),
            &Response {
                code: ResponseCode::Ok,
                generation: None,
                message: "state=down".to_owned(),
                status: Some(StatusSnapshot::from_machine(&StateMachine::default())),
            },
        )
        .await?;
        Ok::<(), Box<dyn Error>>(())
    };
    let client = contact(&service, &action, Operation::Status);
    let (server_result, client_result) = tokio::join!(server, client);
    server_result?;
    let (record, failure) = client_result?;
    assert_eq!(failure, None);
    assert_eq!(
        record,
        OutputRecord {
            scope: RuntimeOrigin::Custom,
            service: "api".to_owned(),
            result: "ok".to_owned(),
            supervisor_pid: None,
            main_pid: None,
            generation: None,
            desired: Some("up".to_owned()),
            state: Some("down".to_owned()),
            readiness: Some("n/a".to_owned()),
            uptime_seconds: None,
            down_seconds: None,
            starts: Some(0),
            failures: Some(0),
            last_result: None,
            backoff_seconds: None,
            logger: Some("n/a".to_owned()),
            logger_last_result: None,
            command: Some(Vec::new()),
            message: "state=down".to_owned(),
        }
    );
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn contact_reports_missing_socket() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let service = custom_service(
        "missing",
        directory.path(),
        directory.path().join("missing.sock"),
        0,
    );
    let action = ControlAction {
        discovery: RuntimeDiscovery::Custom(directory.path().to_owned()),
        output: OutputFormat::Table,
        no_header: false,
        wait_timeout: Duration::from_secs(1),
        no_wait: false,
        target: Target::Service("missing".to_owned()),
        scope: SignalScope::Main,
        signal: None,
    };
    assert!(contact(&service, &action, Operation::Status).await.is_err());
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn mutations_are_bound_to_status_generation() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let socket = directory.path().join("immortal.sock");
    let listener = ControlListener::bind(&socket, 1)?;
    let service = custom_service("api", directory.path(), socket, listener.owner_uid());
    let action = ControlAction {
        discovery: RuntimeDiscovery::Custom(directory.path().to_owned()),
        output: OutputFormat::Table,
        no_header: false,
        wait_timeout: Duration::from_secs(1),
        no_wait: true,
        target: Target::Service("api".to_owned()),
        scope: SignalScope::Main,
        signal: None,
    };

    let generation = immortal_core::supervisor::Generation::FIRST;
    let server = async {
        let mut status_connection = listener.accept().await?;
        let status = read_request(status_connection.stream_mut()).await?;
        assert_eq!(status.operation, Operation::Status);
        assert_eq!(status.expected_generation, GenerationMatch::Any);
        write_response(
            status_connection.stream_mut(),
            &Response {
                code: ResponseCode::Ok,
                generation: Some(generation),
                message: "state=ready".to_owned(),
                status: Some(StatusSnapshot::from_machine(&ready_machine()?)),
            },
        )
        .await?;
        drop(status_connection);

        let mut mutation_connection = listener.accept().await?;
        let mutation = read_request(mutation_connection.stream_mut()).await?;
        assert_eq!(mutation.operation, Operation::Restart);
        assert_eq!(
            mutation.expected_generation,
            GenerationMatch::Exact(generation)
        );
        write_response(
            mutation_connection.stream_mut(),
            &Response {
                code: ResponseCode::Ok,
                generation: Some(generation),
                message: "restart accepted".to_owned(),
                status: None,
            },
        )
        .await?;
        Ok::<(), Box<dyn Error>>(())
    };
    let client = contact(&service, &action, Operation::Restart);
    let (server_result, client_result) = tokio::join!(server, client);
    server_result?;
    let (record, failure) = client_result?;
    assert_eq!(failure, None);
    assert_eq!(record.message, "restart accepted");
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn restart_waits_for_a_new_ready_generation() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let socket = directory.path().join("immortal.sock");
    let listener = ControlListener::bind(&socket, 1)?;
    let service = custom_service("api", directory.path(), socket, listener.owner_uid());
    let action = ControlAction {
        discovery: RuntimeDiscovery::Custom(directory.path().to_owned()),
        output: OutputFormat::Table,
        no_header: false,
        wait_timeout: Duration::from_secs(1),
        no_wait: false,
        target: Target::Service("api".to_owned()),
        scope: SignalScope::Main,
        signal: None,
    };
    let first = Generation::FIRST;
    let second = Generation::new(2).ok_or("invalid generation")?;
    let server = async {
        respond_once(
            &listener,
            response_with_state(Some(first), ServiceState::Ready),
        )
        .await?;

        let mut mutation = listener.accept().await?;
        let request = read_request(mutation.stream_mut()).await?;
        assert_eq!(request.operation, Operation::Restart);
        write_response(
            mutation.stream_mut(),
            &Response {
                code: ResponseCode::Ok,
                generation: Some(first),
                message: "accepted".to_owned(),
                status: None,
            },
        )
        .await?;
        drop(mutation);

        respond_once(
            &listener,
            response_with_state(Some(second), ServiceState::Starting),
        )
        .await?;
        respond_once(
            &listener,
            response_with_state(Some(second), ServiceState::Ready),
        )
        .await?;
        Ok::<(), Box<dyn Error>>(())
    };
    let client = contact(&service, &action, Operation::Restart);
    let (server_result, client_result) = tokio::join!(server, client);
    server_result?;
    let (record, failure) = client_result?;
    assert_eq!(failure, None);
    assert_eq!(record.generation, Some(2));
    assert_eq!(record.state.as_deref(), Some("ready"));
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
async fn lifecycle_wait_has_a_hard_deadline() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let socket = directory.path().join("immortal.sock");
    let listener = ControlListener::bind(&socket, 1)?;
    let service = custom_service("api", directory.path(), socket, listener.owner_uid());
    let action = ControlAction {
        discovery: RuntimeDiscovery::Custom(directory.path().to_owned()),
        output: OutputFormat::Table,
        no_header: false,
        wait_timeout: Duration::from_millis(1),
        no_wait: false,
        target: Target::Service("api".to_owned()),
        scope: SignalScope::Main,
        signal: None,
    };
    let server = async {
        respond_once(&listener, response_with_state(None, ServiceState::Down)).await?;
        let mut mutation = listener.accept().await?;
        let request = read_request(mutation.stream_mut()).await?;
        assert_eq!(request.operation, Operation::Start);
        write_response(
            mutation.stream_mut(),
            &Response {
                code: ResponseCode::Ok,
                generation: None,
                message: "accepted".to_owned(),
                status: None,
            },
        )
        .await?;
        Ok::<(), Box<dyn Error>>(())
    };
    let client = contact(&service, &action, Operation::Start);
    let (server_result, client_result) = tokio::join!(server, client);
    server_result?;
    assert!(matches!(client_result, Err(ActionError::LifecycleTimeout)));
    Ok(())
}

/// Regression: every reply was awaited for the five-second idle deadline, so
/// stopping a descriptor-tracked service whose stop hook ran longer failed
/// although the stop was still in progress and within its own limit.
#[tokio::test(flavor = "current_thread")]
async fn a_withheld_reply_is_awaited_for_the_given_time() -> Result<(), Box<dyn Error>> {
    // The supervisor withholds its reply this long; the two client budgets
    // sit well on either side of it, so scheduling cannot change the result.
    const WITHHELD: Duration = Duration::from_millis(200);
    let directory = TestDirectory::new()?;
    let socket = directory.path().join("immortal.sock");
    let listener = ControlListener::bind(&socket, 1)?;
    let service = custom_service("api", directory.path(), socket, listener.owner_uid());
    let server = async {
        for _ in 0..2 {
            let mut mutation = listener.accept().await?;
            let request = read_request(mutation.stream_mut()).await?;
            assert_eq!(request.operation, Operation::Stop);
            sleep(WITHHELD).await;
            // The impatient client has already gone; only the write to the
            // patient one must succeed.
            let _ = write_response(
                mutation.stream_mut(),
                &response_with_state(None, ServiceState::Down),
            )
            .await;
        }
        Ok::<(), Box<dyn Error>>(())
    };
    let client = async {
        let impatient = exchange(
            &service.service,
            Operation::Stop,
            GenerationMatch::NoChild,
            SignalScope::Group,
            None,
            WITHHELD / 10,
        )
        .await;
        let patient = exchange(
            &service.service,
            Operation::Stop,
            GenerationMatch::NoChild,
            SignalScope::Group,
            None,
            WITHHELD * 50,
        )
        .await;
        (impatient, patient)
    };
    let (server_result, (impatient, patient)) = tokio::join!(server, client);
    server_result?;
    assert!(matches!(
        impatient,
        Err(ActionError::Transport(TransportError::Timeout))
    ));
    assert_eq!(patient?.code, ResponseCode::Ok);
    Ok(())
}

#[test]
fn mutation_replies_use_the_lifecycle_budget_and_status_does_not() {
    let budget = Duration::from_secs(90);
    assert_eq!(reply_timeout(Operation::Status, budget), CONTROL_IO_TIMEOUT);
    for operation in [
        Operation::Stop,
        Operation::Restart,
        Operation::Halt,
        Operation::Signal,
    ] {
        assert_eq!(reply_timeout(operation, budget), budget);
        assert_eq!(
            reply_timeout(operation, Duration::from_millis(1)),
            CONTROL_IO_TIMEOUT,
            "a short budget never waits less than the default deadline"
        );
        assert!(matches!(
            reply_error(operation, TransportError::Timeout.into()),
            ActionError::LifecycleTimeout
        ));
    }
    assert!(matches!(
        reply_error(Operation::Status, TransportError::Timeout.into()),
        ActionError::Transport(TransportError::Timeout)
    ));
}

#[test]
fn structured_status_renders_as_table_and_json() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let service = custom_service(
        "api",
        directory.path(),
        directory.path().join("immortal.sock"),
        0,
    );
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.supervisor_pid = Some(101);
    status.down_seconds = Some(7);
    status.command = vec!["/usr/bin/api".to_owned(), "argument with spaces".to_owned()];
    let output_record = record(
        &service,
        Response {
            code: ResponseCode::Ok,
            generation: None,
            message: "healthy\nnow".to_owned(),
            status: Some(status),
        },
    );

    let mut json = Vec::new();
    render_output(
        &mut json,
        std::slice::from_ref(&output_record),
        OutputFormat::Json,
        false,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&json)?;
    let first = value
        .as_array()
        .and_then(|records| records.first())
        .ok_or_else(|| std::io::Error::other("status JSON record missing"))?;
    assert_eq!(first.get("supervisor_pid"), Some(&serde_json::json!(101)));
    assert_eq!(first.get("scope"), Some(&serde_json::json!("custom")));
    assert_eq!(first.get("state"), Some(&serde_json::json!("down")));
    assert_eq!(
        first
            .get("command")
            .and_then(serde_json::Value::as_array)
            .and_then(|arguments| arguments.get(1)),
        Some(&serde_json::json!("argument with spaces"))
    );

    let mut table = Vec::new();
    render_output(&mut table, &[output_record], OutputFormat::Table, false)?;
    let table = String::from_utf8(table)?;
    assert!(table.starts_with("SCOPE\tSERVICE\tRESULT\tSUPERVISOR"));
    assert!(table.contains("\"argument with spaces\""));
    assert!(!table.contains("healthy\nnow"));
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

fn response_with_state(generation: Option<Generation>, state: ServiceState) -> Response {
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.state = state;
    Response {
        code: ResponseCode::Ok,
        generation,
        message: format!("state={}", state.name()),
        status: Some(status),
    }
}

async fn respond_once(
    listener: &ControlListener,
    response: Response,
) -> Result<(), Box<dyn Error>> {
    let mut connection = listener.accept().await?;
    let request = read_request(connection.stream_mut()).await?;
    assert_eq!(request.operation, Operation::Status);
    write_response(connection.stream_mut(), &response).await?;
    Ok(())
}

/// The table must say, in its own columns and in words, why a service or its
/// logger could not start; a status without a failure adds nothing.
#[test]
fn record_explains_spawn_and_logger_failures() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let service = custom_service("api", directory.path(), directory.path().join("s"), 0);
    let detail = SpawnDetail {
        stage: SpawnStage::Execute,
        failure: SpawnFailure::OperatingSystem,
        os_error: Some(2),
    };
    let mut status = snapshot(ServiceState::Backoff);
    status.last_result = Some(LastResult::SpawnFailed);
    status.last_spawn_failure = Some(detail);
    status.logger = LoggerStatus::Backoff;
    status.logger_last_result = Some(LoggerLastResult::SpawnFailed(detail));
    let explained = record(
        &service,
        Response {
            code: ResponseCode::Ok,
            generation: None,
            message: "state=backoff".to_owned(),
            status: Some(status),
        },
    );
    assert_eq!(
        explained.last_result.as_deref(),
        Some("spawn-failed:execute:os-error:2")
    );
    assert_eq!(
        explained.logger_last_result.as_deref(),
        Some("spawn-failed:execute:os-error:2")
    );
    assert!(
        explained
            .message
            .starts_with("state=backoff; start: execution failed: ")
    );
    assert!(explained.message.contains("; logger: execution failed: "));

    let mut json = Vec::new();
    render_output(
        &mut json,
        std::slice::from_ref(&explained),
        OutputFormat::Json,
        false,
    )?;
    let value: serde_json::Value = serde_json::from_slice(&json)?;
    let first = value
        .as_array()
        .and_then(|records| records.first())
        .ok_or("status JSON record missing")?;
    for field in ["last_result", "logger_last_result"] {
        assert_eq!(
            first.get(field),
            Some(&serde_json::json!("spawn-failed:execute:os-error:2")),
            "{field}"
        );
    }

    let mut output = Vec::new();
    render_output(&mut output, &[explained], OutputFormat::Table, false)?;
    let table = String::from_utf8(output)?;
    let mut lines = table.lines();
    let header: Vec<&str> = lines.next().ok_or("missing header")?.split('\t').collect();
    let row: Vec<&str> = lines.next().ok_or("missing row")?.split('\t').collect();
    assert_eq!(header.len(), row.len());
    let column = header
        .iter()
        .position(|name| *name == "LOGGER_LAST")
        .ok_or("LOGGER_LAST column missing")?;
    assert_eq!(header.get(column - 1), Some(&"LOGGER"));
    assert_eq!(row.get(column), Some(&"spawn-failed:execute:os-error:2"));

    let plain = record(
        &service,
        Response {
            code: ResponseCode::Ok,
            generation: None,
            message: "state=down".to_owned(),
            status: Some(snapshot(ServiceState::Down)),
        },
    );
    assert_eq!(plain.logger_last_result, None);
    assert_eq!(plain.message, "state=down");
    let mut json = Vec::new();
    render_output(&mut json, &[plain], OutputFormat::Json, false)?;
    let value: serde_json::Value = serde_json::from_slice(&json)?;
    assert_eq!(
        value
            .as_array()
            .and_then(|records| records.first())
            .and_then(|record| record.get("logger_last_result")),
        Some(&serde_json::Value::Null),
        "an absent logger failure is an explicit null"
    );
    Ok(())
}

fn snapshot(state: ServiceState) -> StatusSnapshot {
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.state = state;
    status
}

/// Regression: a settled supervisor stalled the client for the whole deadline.
///
/// `start`, `restart`, and `once` waited for their goal state and nothing else,
/// so a supervisor which had stopped restarting was polled until `--timeout`
/// expired and then reported as a retryable temporary failure. A supervisor in
/// `failed` or `exited` will not reach a start goal on its own, so the client
/// must give up immediately and report a definite failure instead.
#[test]
fn a_settled_supervisor_abandons_a_start_goal_immediately() {
    for operation in [Operation::Start, Operation::Restart, Operation::Once] {
        for state in [ServiceState::Failed, ServiceState::Exited] {
            let mut saw_once_generation = false;
            assert_eq!(
                operation_outcome(
                    operation,
                    None,
                    None,
                    &snapshot(state),
                    &mut saw_once_generation
                ),
                WaitOutcome::Abandoned,
                "{operation:?} in {state:?} must not wait out the deadline"
            );
        }
    }
}

/// Neither settled state runs a child, so a stop goal is already satisfied.
#[test]
fn a_settled_supervisor_satisfies_a_stop_goal() {
    for state in [
        ServiceState::Failed,
        ServiceState::Exited,
        ServiceState::Down,
    ] {
        let mut saw_once_generation = false;
        assert_eq!(
            operation_outcome(
                Operation::Stop,
                None,
                None,
                &snapshot(state),
                &mut saw_once_generation
            ),
            WaitOutcome::Completed,
            "stop in {state:?} must complete"
        );
    }
}

/// A supervisor still working toward the goal keeps the client waiting.
#[test]
fn a_working_supervisor_keeps_the_client_waiting() {
    for state in [
        ServiceState::Starting,
        ServiceState::Running,
        ServiceState::Backoff,
        ServiceState::WaitingCondition,
    ] {
        let mut saw_once_generation = false;
        assert_eq!(
            operation_outcome(
                Operation::Start,
                None,
                None,
                &snapshot(state),
                &mut saw_once_generation
            ),
            WaitOutcome::Pending,
            "start in {state:?} must keep polling"
        );
    }
    let mut saw_once_generation = false;
    assert_eq!(
        operation_outcome(
            Operation::Start,
            None,
            None,
            &snapshot(ServiceState::Ready),
            &mut saw_once_generation
        ),
        WaitOutcome::Completed
    );
}

/// The abandoned failure is definite, not the retryable timeout class.
#[test]
fn an_abandoned_lifecycle_is_reported_as_unavailable() {
    assert_eq!(
        ActionError::LifecycleAbandoned(ServiceState::Failed).exit_class(),
        ExitClass::Unavailable
    );
    assert_ne!(
        ActionError::LifecycleAbandoned(ServiceState::Failed).exit_class(),
        ActionError::LifecycleTimeout.exit_class()
    );
}
