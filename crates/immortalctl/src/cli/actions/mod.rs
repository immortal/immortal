//! Runtime discovery, bounded control requests, and stable client output.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
};

use immortal_core::{
    control::{
        CONTROL_IO_TIMEOUT, GenerationMatch, Operation, Request, Response, ResponseCode, Signal,
        SignalScope, TransportError, read_response, write_request,
    },
    exit::ExitClass,
    runtime::{RuntimeRootError, RuntimeService, discover},
    status::{ServiceState, StatusSnapshot, desired_state_name},
    supervisor::Generation,
};
use serde::Serialize;
use tokio::{
    net::UnixStream,
    time::{sleep, timeout},
};

use crate::cli::dispatch::{Action, OutputFormat, Target};

const LIFECYCLE_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

/// Failure while discovering or contacting supervisors.
#[derive(Debug)]
pub enum ActionError {
    /// Runtime root is absent or unsafe.
    Runtime(RuntimeRootError),
    /// Requested service was not safely discovered.
    ServiceNotFound(String),
    /// Unix socket connection failed.
    Connect(io::Error),
    /// Connect deadline elapsed.
    ConnectTimeout,
    /// Lifecycle did not reach its requested terminal state before the deadline.
    LifecycleTimeout,
    /// A successful status response omitted its typed payload.
    StatusUnavailable,
    /// Framed request or response failed.
    Transport(TransportError),
    /// Human or machine-readable output failed.
    Output(io::Error),
    /// JSON serialization failed.
    Json(serde_json::Error),
    /// One supervisor returned a stable non-success result.
    Remote(ExitClass),
    /// At least one supervisor rejected or failed an operation.
    PartialFailure,
}

impl Display for ActionError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Runtime(error) => Display::fmt(error, formatter),
            Self::ServiceNotFound(service) => {
                write!(formatter, "service `{service}` was not safely discovered")
            }
            Self::Connect(error) => write!(formatter, "unable to connect to supervisor: {error}"),
            Self::ConnectTimeout => formatter.write_str("supervisor connect deadline exceeded"),
            Self::LifecycleTimeout => formatter.write_str("lifecycle completion deadline exceeded"),
            Self::StatusUnavailable => {
                formatter.write_str("supervisor omitted the typed status payload")
            }
            Self::Transport(error) => Display::fmt(error, formatter),
            Self::Output(error) => write!(formatter, "unable to write output: {error}"),
            Self::Json(error) => write!(formatter, "unable to serialize JSON output: {error}"),
            Self::Remote(class) => {
                write!(
                    formatter,
                    "supervisor rejected the operation ({})",
                    class.value()
                )
            }
            Self::PartialFailure => formatter.write_str("one or more control operations failed"),
        }
    }
}

impl Error for ActionError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Runtime(error) => Some(error),
            Self::Connect(error) | Self::Output(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Json(error) => Some(error),
            Self::ServiceNotFound(_)
            | Self::ConnectTimeout
            | Self::LifecycleTimeout
            | Self::StatusUnavailable
            | Self::Remote(_)
            | Self::PartialFailure => None,
        }
    }
}

impl From<RuntimeRootError> for ActionError {
    fn from(error: RuntimeRootError) -> Self {
        Self::Runtime(error)
    }
}

impl From<TransportError> for ActionError {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

impl ActionError {
    /// Stable process exit classification for this failure.
    #[must_use]
    pub fn exit_class(&self) -> ExitClass {
        match self {
            Self::Runtime(_) => ExitClass::Configuration,
            Self::ServiceNotFound(_) => ExitClass::NotFound,
            Self::Connect(error) => match error.kind() {
                io::ErrorKind::PermissionDenied => ExitClass::Permission,
                io::ErrorKind::NotFound
                | io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset => ExitClass::Unavailable,
                _ => ExitClass::OsError,
            },
            Self::ConnectTimeout
            | Self::LifecycleTimeout
            | Self::Transport(TransportError::Timeout) => ExitClass::TemporaryFailure,
            Self::StatusUnavailable | Self::Transport(TransportError::Protocol(_)) => {
                ExitClass::Data
            }
            Self::Transport(TransportError::Io(_)) | Self::Output(_) => ExitClass::IoError,
            Self::Json(_) => ExitClass::Software,
            Self::Remote(class) => *class,
            Self::PartialFailure => ExitClass::PartialFailure,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct OutputRecord {
    service: String,
    result: String,
    supervisor_pid: Option<u32>,
    main_pid: Option<u32>,
    generation: Option<u64>,
    desired: Option<String>,
    state: Option<String>,
    readiness: Option<String>,
    uptime_seconds: Option<u64>,
    down_seconds: Option<u64>,
    starts: Option<u64>,
    failures: Option<u64>,
    last_result: Option<String>,
    backoff_seconds: Option<u64>,
    logger: Option<String>,
    command: Option<Vec<String>>,
    message: String,
}

/// Discover targets, send one bounded request to each, and render responses.
///
/// # Errors
///
/// Returns an error for unsafe discovery, missing targets, connection or
/// transport failure, rejected operations, or output failure.
pub async fn execute(action: &Action) -> Result<(), ActionError> {
    let discovery = discover(&action.runtime_directory)?;
    let mut diagnostics = io::stderr().lock();
    for problem in discovery.problems {
        writeln!(
            diagnostics,
            "{}: ignored runtime entry: {:?}",
            problem.path.display(),
            problem.kind
        )
        .map_err(ActionError::Output)?;
    }

    let targets: Vec<RuntimeService> = match &action.target {
        Target::All => discovery.services.into_values().collect(),
        Target::Service(name) => vec![
            discovery
                .services
                .get(name)
                .cloned()
                .ok_or_else(|| ActionError::ServiceNotFound(name.clone()))?,
        ],
    };
    if targets.is_empty() && action.operation != Operation::Status {
        return Err(ActionError::ServiceNotFound("*".to_owned()));
    }

    let mut records = Vec::with_capacity(targets.len());
    let target_count = targets.len();
    let mut failure_class = None;
    for service in targets {
        match contact(&service, action).await {
            Ok((record, failure)) => {
                if let Some(class) = failure {
                    failure_class = Some(class);
                }
                records.push(record);
            }
            Err(error) => {
                failure_class = Some(error.exit_class());
                records.push(OutputRecord {
                    service: service.name,
                    result: "transport-error".to_owned(),
                    supervisor_pid: None,
                    main_pid: None,
                    generation: None,
                    desired: None,
                    state: None,
                    readiness: None,
                    uptime_seconds: None,
                    down_seconds: None,
                    starts: None,
                    failures: None,
                    last_result: None,
                    backoff_seconds: None,
                    logger: None,
                    command: None,
                    message: error.to_string(),
                });
            }
        }
    }
    write_output(&records, action.output, action.no_header)?;
    if failure_class.is_some() && target_count > 1 {
        Err(ActionError::PartialFailure)
    } else if let Some(class) = failure_class {
        Err(ActionError::Remote(class))
    } else {
        Ok(())
    }
}

async fn contact(
    service: &RuntimeService,
    action: &Action,
) -> Result<(OutputRecord, Option<ExitClass>), ActionError> {
    let (expected_generation, initial_generation) = if action.operation == Operation::Status {
        (GenerationMatch::Any, None)
    } else {
        let status = exchange(
            service,
            Operation::Status,
            GenerationMatch::Any,
            SignalScope::Main,
            None,
        )
        .await?;
        if !status.code.is_success() {
            let class = response_exit_class(status.code);
            return Ok((record(service, status), Some(class)));
        }
        let generation = status.generation;
        (
            generation.map_or(GenerationMatch::NoChild, GenerationMatch::Exact),
            generation,
        )
    };
    let mut response = exchange(
        service,
        action.operation,
        expected_generation,
        action.scope,
        action.signal,
    )
    .await?;
    if response.code.is_success()
        && !action.no_wait
        && matches!(
            action.operation,
            Operation::Start
                | Operation::Stop
                | Operation::Restart
                | Operation::Once
                | Operation::Exit
                | Operation::Halt
        )
    {
        response = timeout(
            action.wait_timeout,
            wait_for_completion(service, action.operation, initial_generation, response),
        )
        .await
        .map_err(|_| ActionError::LifecycleTimeout)??;
    }
    let failure = (!response.code.is_success()).then_some(response_exit_class(response.code));
    Ok((record(service, response), failure))
}

async fn wait_for_completion(
    service: &RuntimeService,
    operation: Operation,
    initial_generation: Option<Generation>,
    accepted: Response,
) -> Result<Response, ActionError> {
    let mut saw_once_generation = false;
    loop {
        sleep(LIFECYCLE_POLL_INTERVAL).await;
        let response = match exchange(
            service,
            Operation::Status,
            GenerationMatch::Any,
            SignalScope::Main,
            None,
        )
        .await
        {
            Ok(response) => response,
            Err(ActionError::Connect(error))
                if matches!(operation, Operation::Exit | Operation::Halt)
                    && matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::ConnectionReset
                    ) =>
            {
                return Ok(accepted);
            }
            Err(error) => return Err(error),
        };
        if !response.code.is_success() {
            return Ok(response);
        }
        let status = response
            .status
            .as_ref()
            .ok_or(ActionError::StatusUnavailable)?;
        if operation_completed(
            operation,
            initial_generation,
            response.generation,
            status,
            &mut saw_once_generation,
        ) {
            return Ok(response);
        }
    }
}

fn operation_completed(
    operation: Operation,
    initial_generation: Option<Generation>,
    current_generation: Option<Generation>,
    status: &StatusSnapshot,
    saw_once_generation: &mut bool,
) -> bool {
    match operation {
        Operation::Start => status.state == ServiceState::Ready,
        Operation::Stop => status.state == ServiceState::Down,
        Operation::Restart => {
            status.state == ServiceState::Ready && current_generation != initial_generation
        }
        Operation::Once => {
            if current_generation.is_some() || status.state != ServiceState::Down {
                *saw_once_generation = true;
            }
            *saw_once_generation && status.state == ServiceState::Down
        }
        Operation::Exit | Operation::Halt => false,
        Operation::Status | Operation::Signal => true,
    }
}

async fn exchange(
    service: &RuntimeService,
    operation: Operation,
    expected_generation: GenerationMatch,
    scope: SignalScope,
    signal: Option<Signal>,
) -> Result<Response, ActionError> {
    let mut stream = timeout(CONTROL_IO_TIMEOUT, UnixStream::connect(&service.socket))
        .await
        .map_err(|_| ActionError::ConnectTimeout)?
        .map_err(ActionError::Connect)?;
    let request = Request {
        operation,
        service: service.name.clone(),
        expected_generation,
        scope,
        signal,
    };
    write_request(&mut stream, &request).await?;
    read_response(&mut stream).await.map_err(ActionError::from)
}

fn record(service: &RuntimeService, response: Response) -> OutputRecord {
    let status = response.status.as_ref();
    OutputRecord {
        service: service.name.clone(),
        result: response.code.name().to_owned(),
        supervisor_pid: status.and_then(|value| value.supervisor_pid),
        main_pid: status.and_then(|value| value.main_pid),
        generation: response.generation.map(Generation::get),
        desired: status.map(|value| desired_state_name(value.desired).to_owned()),
        state: status.map(|value| value.state.name().to_owned()),
        readiness: status.map(|value| value.readiness.name().to_owned()),
        uptime_seconds: status.and_then(|value| value.uptime_seconds),
        down_seconds: status.and_then(|value| value.down_seconds),
        starts: status.map(|value| value.starts),
        failures: status.map(|value| value.failures),
        last_result: status.and_then(|value| {
            value
                .last_result
                .map(immortal_core::status::LastResult::name)
        }),
        backoff_seconds: status.and_then(|value| value.backoff_seconds),
        logger: status.map(|value| value.logger.name().to_owned()),
        command: status.map(|value| value.command.clone()),
        message: response.message,
    }
}

const fn response_exit_class(code: ResponseCode) -> ExitClass {
    match code {
        ResponseCode::Ok => ExitClass::Success,
        ResponseCode::NotFound => ExitClass::NotFound,
        ResponseCode::PermissionDenied => ExitClass::Permission,
        ResponseCode::Conflict => ExitClass::TemporaryFailure,
        ResponseCode::Invalid => ExitClass::Data,
        ResponseCode::Internal => ExitClass::Software,
    }
}

fn write_output(
    records: &[OutputRecord],
    format: OutputFormat,
    no_header: bool,
) -> Result<(), ActionError> {
    let mut output = io::stdout().lock();
    render_output(&mut output, records, format, no_header)
}

fn render_output(
    output: &mut impl Write,
    records: &[OutputRecord],
    format: OutputFormat,
    no_header: bool,
) -> Result<(), ActionError> {
    match format {
        OutputFormat::Json => {
            serde_json::to_writer(&mut *output, records).map_err(ActionError::Json)?;
            writeln!(output).map_err(ActionError::Output)
        }
        OutputFormat::Table => {
            if !no_header {
                writeln!(
                    output,
                    "SERVICE\tRESULT\tSUPERVISOR\tMAIN\tGENERATION\tDESIRED\tSTATE\tREADINESS\tUP\tDOWN\tSTARTS\tFAILURES\tLAST\tBACKOFF\tLOGGER\tCOMMAND\tMESSAGE"
                )
                .map_err(ActionError::Output)?;
            }
            for record in records {
                let command = record.command.as_ref().map(|arguments| {
                    arguments
                        .iter()
                        .map(|argument| format!("{argument:?}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                });
                writeln!(
                    output,
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    record.service,
                    record.result,
                    display_option(record.supervisor_pid),
                    display_option(record.main_pid),
                    display_option(record.generation),
                    display_text(record.desired.as_deref()),
                    display_text(record.state.as_deref()),
                    display_text(record.readiness.as_deref()),
                    display_option(record.uptime_seconds),
                    display_option(record.down_seconds),
                    display_option(record.starts),
                    display_option(record.failures),
                    display_text(record.last_result.as_deref()),
                    display_option(record.backoff_seconds),
                    display_text(record.logger.as_deref()),
                    sanitize(command.as_deref().unwrap_or("-")),
                    sanitize(&record.message),
                )
                .map_err(ActionError::Output)?;
            }
            Ok(())
        }
    }
}

fn display_option(value: Option<impl Display>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| value.to_string())
}

fn display_text(value: Option<&str>) -> String {
    sanitize(value.unwrap_or("-"))
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
        time::Duration,
    };

    use immortal_core::control::{
        ControlListener, GenerationMatch, Operation, Response, ResponseCode, SignalScope,
        read_request, write_response,
    };
    use immortal_core::status::{ServiceState, StatusSnapshot};
    use immortal_core::supervisor::{Generation, StateMachine};

    use super::{ActionError, OutputRecord, contact, record, render_output};
    use crate::cli::dispatch::{Action, OutputFormat, Target};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortalctl-action-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
            Ok(Self(path))
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

    #[tokio::test(flavor = "current_thread")]
    async fn contact_sends_typed_request_and_receives_status() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let socket = directory.path().join("immortal.sock");
        let listener = ControlListener::bind(&socket, 1)?;
        let service = immortal_core::runtime::RuntimeService {
            name: "api".to_owned(),
            directory: directory.path().to_owned(),
            socket,
            owner_uid: listener.owner_uid(),
        };
        let action = Action {
            runtime_directory: directory.path().to_owned(),
            output: OutputFormat::Table,
            no_header: false,
            wait_timeout: Duration::from_secs(1),
            no_wait: false,
            operation: Operation::Status,
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
        let client = contact(&service, &action);
        let (server_result, client_result) = tokio::join!(server, client);
        server_result?;
        let (record, failure) = client_result?;
        assert_eq!(failure, None);
        assert_eq!(
            record,
            OutputRecord {
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
                command: Some(Vec::new()),
                message: "state=down".to_owned(),
            }
        );
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn contact_reports_missing_socket() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let service = immortal_core::runtime::RuntimeService {
            name: "missing".to_owned(),
            directory: directory.path().to_owned(),
            socket: directory.path().join("missing.sock"),
            owner_uid: 0,
        };
        let action = Action {
            runtime_directory: directory.path().to_owned(),
            output: OutputFormat::Table,
            no_header: false,
            wait_timeout: Duration::from_secs(1),
            no_wait: false,
            operation: Operation::Status,
            target: Target::Service("missing".to_owned()),
            scope: SignalScope::Main,
            signal: None,
        };
        assert!(contact(&service, &action).await.is_err());
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mutations_are_bound_to_status_generation() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let socket = directory.path().join("immortal.sock");
        let listener = ControlListener::bind(&socket, 1)?;
        let service = immortal_core::runtime::RuntimeService {
            name: "api".to_owned(),
            directory: directory.path().to_owned(),
            socket,
            owner_uid: listener.owner_uid(),
        };
        let action = Action {
            runtime_directory: directory.path().to_owned(),
            output: OutputFormat::Table,
            no_header: false,
            wait_timeout: Duration::from_secs(1),
            no_wait: true,
            operation: Operation::Restart,
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
        let client = contact(&service, &action);
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
        let service = immortal_core::runtime::RuntimeService {
            name: "api".to_owned(),
            directory: directory.path().to_owned(),
            socket,
            owner_uid: listener.owner_uid(),
        };
        let action = Action {
            runtime_directory: directory.path().to_owned(),
            output: OutputFormat::Table,
            no_header: false,
            wait_timeout: Duration::from_secs(1),
            no_wait: false,
            operation: Operation::Restart,
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
        let client = contact(&service, &action);
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
        let service = immortal_core::runtime::RuntimeService {
            name: "api".to_owned(),
            directory: directory.path().to_owned(),
            socket,
            owner_uid: listener.owner_uid(),
        };
        let action = Action {
            runtime_directory: directory.path().to_owned(),
            output: OutputFormat::Table,
            no_header: false,
            wait_timeout: Duration::from_millis(1),
            no_wait: false,
            operation: Operation::Start,
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
        let client = contact(&service, &action);
        let (server_result, client_result) = tokio::join!(server, client);
        server_result?;
        assert!(matches!(client_result, Err(ActionError::LifecycleTimeout)));
        Ok(())
    }

    #[test]
    fn structured_status_renders_as_table_and_json() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let service = immortal_core::runtime::RuntimeService {
            name: "api".to_owned(),
            directory: directory.path().to_owned(),
            socket: directory.path().join("immortal.sock"),
            owner_uid: 0,
        };
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
        assert!(table.starts_with("SERVICE\tRESULT\tSUPERVISOR"));
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
}
