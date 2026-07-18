//! Shared runtime discovery, bounded control requests, and stable client output.

use std::{
    error::Error,
    fmt::Display,
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use immortal_core::{
    control::{
        CONTROL_IO_TIMEOUT, GenerationMatch, Operation, Request, Response, ResponseCode, Signal,
        SignalScope, read_response, write_request,
    },
    exit::ExitClass,
    runtime::{
        MAX_RUNTIME_SERVICES, RuntimeRootError, RuntimeService, discover, discover_user,
        system_runtime_root, user_runtime_root,
    },
    status::{ServiceState, StatusSnapshot, desired_state_name},
    supervisor::Generation,
};
use serde::Serialize;
use tokio::{
    net::UnixStream,
    runtime::Builder,
    time::{sleep, timeout},
};

use super::{ActionError, ControlAction, OutputFormat, RuntimeDiscovery, RuntimeScope, Target};

const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
enum RuntimeOrigin {
    System,
    User,
    Custom,
}

impl RuntimeOrigin {
    const fn name(self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Custom => "custom",
        }
    }
}

struct ScopedService {
    origin: RuntimeOrigin,
    service: RuntimeService,
}

struct DiscoveryRoot {
    origin: RuntimeOrigin,
    path: PathBuf,
    user_owned: bool,
    optional: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct OutputRecord {
    scope: RuntimeOrigin,
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

pub(super) fn execute(action: &ControlAction, operation: Operation) -> Result<(), ActionError> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ActionError::RuntimeInitialization)?;
    runtime.block_on(run(action, operation))
}

async fn run(action: &ControlAction, operation: Operation) -> Result<(), ActionError> {
    let mut diagnostics = io::stderr().lock();
    let (services, discovery_failed) = discover_selected(action, &mut diagnostics)?;
    let targets = select_targets(services, &action.target, discovery_failed)?;
    if targets.is_empty() && operation != Operation::Status {
        return Err(ActionError::ServiceNotFound("*".to_owned()));
    }

    let mut records = Vec::with_capacity(targets.len());
    let target_count = targets.len();
    let mut failure_class = discovery_failed.then_some(ExitClass::PartialFailure);
    for service in targets {
        match contact(&service, action, operation).await {
            Ok((record, failure)) => {
                if let Some(class) = failure {
                    failure_class = Some(class);
                }
                records.push(record);
            }
            Err(error) => {
                failure_class = Some(error.exit_class());
                records.push(OutputRecord {
                    scope: service.origin,
                    service: service.service.name,
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
    if discovery_failed || (failure_class.is_some() && target_count > 1) {
        Err(ActionError::PartialFailure)
    } else if let Some(class) = failure_class {
        Err(ActionError::Remote(class))
    } else {
        Ok(())
    }
}

fn discover_selected(
    action: &ControlAction,
    diagnostics: &mut impl Write,
) -> Result<(Vec<ScopedService>, bool), ActionError> {
    let (roots, user_error) = discovery_roots(&action.discovery);
    let user_failed = user_error.is_some();
    if let Some(error) = user_error {
        writeln!(diagnostics, "ignored user runtime root: {error}").map_err(ActionError::Output)?;
    }
    let (services, discovery_failed) = discover_from_roots(roots, diagnostics)?;
    Ok((services, user_failed || discovery_failed))
}

fn discover_from_roots(
    roots: Vec<DiscoveryRoot>,
    diagnostics: &mut impl Write,
) -> Result<(Vec<ScopedService>, bool), ActionError> {
    let mut services = Vec::new();
    let mut failed = false;
    for root in roots {
        let discovery = if root.user_owned {
            discover_user(&root.path)
        } else {
            discover(&root.path)
        };
        let discovery = match discovery {
            Ok(discovery) => discovery,
            Err(error) if root.optional && runtime_root_missing(&error) => continue,
            Err(error) if root.optional => {
                writeln!(
                    diagnostics,
                    "{}: ignored {} runtime root: {error}",
                    root.path.display(),
                    root.origin.name()
                )
                .map_err(ActionError::Output)?;
                failed = true;
                continue;
            }
            Err(error) => return Err(ActionError::Runtime(error)),
        };
        for problem in discovery.problems {
            writeln!(
                diagnostics,
                "{}: ignored {} runtime entry: {:?}",
                problem.path.display(),
                root.origin.name(),
                problem.kind
            )
            .map_err(ActionError::Output)?;
        }
        let remaining = MAX_RUNTIME_SERVICES.saturating_sub(services.len());
        if discovery.services.len() > remaining {
            return Err(ActionError::ServiceLimit);
        }
        services.extend(
            discovery
                .services
                .into_values()
                .map(|service| ScopedService {
                    origin: root.origin,
                    service,
                }),
        );
    }
    Ok((services, failed))
}

fn discovery_roots(discovery: &RuntimeDiscovery) -> (Vec<DiscoveryRoot>, Option<io::Error>) {
    discovery_roots_with(discovery, user_runtime_root)
}

fn discovery_roots_with(
    discovery: &RuntimeDiscovery,
    resolve_user_root: impl FnOnce() -> io::Result<PathBuf>,
) -> (Vec<DiscoveryRoot>, Option<io::Error>) {
    match discovery {
        RuntimeDiscovery::Custom(path) => (
            vec![DiscoveryRoot {
                origin: RuntimeOrigin::Custom,
                path: path.clone(),
                user_owned: false,
                optional: false,
            }],
            None,
        ),
        RuntimeDiscovery::Automatic(scope) => {
            let mut roots = Vec::with_capacity(2);
            if matches!(scope, RuntimeScope::All | RuntimeScope::System) {
                roots.push(DiscoveryRoot {
                    origin: RuntimeOrigin::System,
                    path: system_runtime_root().to_owned(),
                    user_owned: false,
                    optional: true,
                });
            }
            let mut user_error = None;
            if matches!(scope, RuntimeScope::All | RuntimeScope::User) {
                match resolve_user_root() {
                    Ok(path) => roots.push(DiscoveryRoot {
                        origin: RuntimeOrigin::User,
                        path,
                        user_owned: true,
                        optional: true,
                    }),
                    Err(error) => user_error = Some(error),
                }
            }
            (roots, user_error)
        }
    }
}

fn runtime_root_missing(error: &RuntimeRootError) -> bool {
    error
        .source()
        .and_then(|source| source.downcast_ref::<io::Error>())
        .is_some_and(|error| error.kind() == io::ErrorKind::NotFound)
}

fn select_targets(
    services: Vec<ScopedService>,
    target: &Target,
    discovery_failed: bool,
) -> Result<Vec<ScopedService>, ActionError> {
    let Target::Service(name) = target else {
        return Ok(services);
    };
    let matches: Vec<ScopedService> = services
        .into_iter()
        .filter(|service| service.service.name == *name)
        .collect();
    match matches.len() {
        0 if discovery_failed => Err(ActionError::PartialFailure),
        0 => Err(ActionError::ServiceNotFound(name.clone())),
        1 => Ok(matches),
        _ => Err(ActionError::AmbiguousService(name.clone())),
    }
}

async fn contact(
    service: &ScopedService,
    action: &ControlAction,
    operation: Operation,
) -> Result<(OutputRecord, Option<ExitClass>), ActionError> {
    let (expected_generation, initial_generation) = if operation == Operation::Status {
        (GenerationMatch::Any, None)
    } else {
        let status = exchange(
            &service.service,
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
        &service.service,
        operation,
        expected_generation,
        action.scope,
        action.signal,
    )
    .await?;
    if response.code.is_success()
        && !action.no_wait
        && matches!(
            operation,
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
            wait_for_completion(&service.service, operation, initial_generation, response),
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

fn record(service: &ScopedService, response: Response) -> OutputRecord {
    let status = response.status.as_ref();
    OutputRecord {
        scope: service.origin,
        service: service.service.name.clone(),
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
                    "SCOPE\tSERVICE\tRESULT\tSUPERVISOR\tMAIN\tGENERATION\tDESIRED\tSTATE\tREADINESS\tUP\tDOWN\tSTARTS\tFAILURES\tLAST\tBACKOFF\tLOGGER\tCOMMAND\tMESSAGE"
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
                    "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
                    record.scope.name(),
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
mod tests;
