//! One fresh process proving continuous checked reconciliation and cleanup.

use std::{
    error::Error,
    fs, io,
    os::unix::{fs::PermissionsExt, net::UnixListener as StdUnixListener},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    config::{parse_file, parse_str},
    control::{GenerationMatch, Operation, Request, SignalScope, exchange},
    executor::{DaemonRunOutcome, run_daemon},
    process::{ChildEvent, start_process_broker, wait_for_event},
    reconcile::{
        DefinitionSnapshots, LaunchConcurrency, LauncherError, SupervisorLaunch, SupervisorLauncher,
    },
    runtime::{RuntimeOwner, discover, supervisor_is_active},
};
use immortaldir::cli::{actions, dispatch::Action};
use tokio::{
    runtime::Builder,
    sync::mpsc,
    time::{sleep, timeout},
};

const DEADLINE: Duration = Duration::from_secs(15);

fn main() -> Result<(), Box<dyn Error>> {
    let arguments: Vec<String> = std::env::args().collect();
    if arguments.get(1).map(String::as_str) == Some("--config") {
        return run_fake_immortal(&arguments);
    }

    prove_operational_lifecycle()
}

fn prove_operational_lifecycle() -> Result<(), Box<dyn Error>> {
    let root = TestRoot::new()?;
    let definitions = root.path().join("definitions");
    let runtime = root.path().join("runtime");
    fs::create_dir(&definitions)?;
    fs::create_dir(&runtime)?;
    fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
    fs::write(
        definitions.join("api.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\n",
    )?;
    write_parallel_definitions(&definitions)?;
    prepare_broken_supervisor(&definitions, &runtime)?;
    let definition = definitions.join("api.yml");
    let action = Action {
        directory: definitions,
        runtime_directory: runtime.clone(),
        scan_interval_seconds: 1,
        supervisor_binary: std::env::current_exe()?,
        launch_concurrency: LaunchConcurrency::new(8)?,
        once: false,
        dry_run: false,
    };
    let limit_endpoint = start_process_broker()?;
    let endpoint = start_process_broker()?;
    let broker = endpoint.process();
    let locked_owner = RuntimeOwner::acquire(&runtime.join("locked"))?;
    let broken_control = bind_broken_control(&runtime)?;
    let tokio = Builder::new_current_thread().enable_all().build()?;
    let result = tokio.block_on(run_reconciliation_contract(
        action,
        limit_endpoint,
        endpoint,
        broken_control,
        locked_owner,
        &definition,
        runtime,
    ));
    drop(tokio);
    let broker_result = require_broker_exit(broker, "watch");
    result.and(broker_result)
}

async fn run_reconciliation_contract(
    action: Action,
    limit_endpoint: immortal_core::process::ProcessBrokerEndpoint,
    endpoint: immortal_core::process::ProcessBrokerEndpoint,
    broken_control: StdUnixListener,
    locked_owner: RuntimeOwner,
    definition: &Path,
    runtime: PathBuf,
) -> Result<(), Box<dyn Error>> {
    prove_batch_limit(limit_endpoint).await?;
    let (attempt_sender, mut attempts) = mpsc::channel(32);
    let broken_task = tokio::spawn(reject_control_clients(broken_control, attempt_sender));
    let task = tokio::spawn(async move { actions::execute(&action, Some(endpoint)).await });
    let result = exercise_reconciliation(definition, &runtime, &mut attempts, locked_owner).await;
    task.abort();
    let _cancelled = task.await;
    let cleanup = halt_services(&runtime).await;
    broken_task.abort();
    let _cancelled = broken_task.await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

fn require_broker_exit(
    broker: immortal_core::process::ProcessId,
    purpose: &str,
) -> Result<(), Box<dyn Error>> {
    match wait_for_event(broker)? {
        ChildEvent::Exited { code: 0, .. } => Ok(()),
        event => Err(format!("{purpose} launch broker exited unexpectedly: {event:?}").into()),
    }
}

async fn prove_batch_limit(
    endpoint: immortal_core::process::ProcessBrokerEndpoint,
) -> Result<(), Box<dyn Error>> {
    let mut launcher = SupervisorLauncher::connect(endpoint).await?;
    if !launcher
        .launch_batch(
            Path::new("/unused/immortal"),
            Vec::new(),
            LaunchConcurrency::new(1)?,
        )
        .await?
        .is_empty()
    {
        return Err("empty supervisor-launch batch produced outcomes".into());
    }
    let specifications = vec![
        SupervisorLaunch::new(PathBuf::from("/unused/a"), PathBuf::from("/unused/a")),
        SupervisorLaunch::new(PathBuf::from("/unused/b"), PathBuf::from("/unused/b")),
    ];
    let result = launcher
        .launch_batch(
            Path::new("/unused/immortal"),
            specifications,
            LaunchConcurrency::new(1)?,
        )
        .await;
    if !matches!(
        result,
        Err(LauncherError::BatchLimit {
            actual: 2,
            limit: 1
        })
    ) {
        return Err("oversized supervisor-launch batch was accepted".into());
    }
    launcher.shutdown().await?;
    Ok(())
}

fn write_parallel_definitions(directory: &Path) -> io::Result<()> {
    let root = directory
        .parent()
        .ok_or_else(|| io::Error::other("definitions directory has no test root"))?;
    for name in ["parallel-a", "parallel-b"] {
        let marker = root.join(format!(".{name}-service"));
        let marker = marker
            .to_str()
            .ok_or_else(|| io::Error::other("parallel marker path is not UTF-8"))?;
        fs::write(
            directory.join(format!("{name}.yml")),
            format!(
                "version: 2\ncommand: [/bin/sh, -c, 'touch \"$MARKER\"; exec /bin/sleep 30']\nenvironment:\n  MARKER: {marker:?}\n"
            ),
        )?;
    }
    fs::write(
        directory.join("dependent.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\nrequires: [parallel-a, parallel-b]\n",
    )?;
    fs::write(
        directory.join("launch-failure.yml"),
        "version: 2\ncommand: [/bin/true]\n",
    )?;
    fs::write(
        directory.join("locked.yml"),
        "version: 2\ncommand: [/bin/sleep, '30']\n",
    )?;
    let condition_marker = root.join(".condition-ready");
    let condition_marker = condition_marker
        .to_str()
        .ok_or_else(|| io::Error::other("condition marker path is not UTF-8"))?;
    fs::write(
        directory.join("conditioned.yml"),
        format!(
            "version: 2\ncommand: [/bin/sleep, '30']\nenvironment:\n  CONDITION: {condition_marker:?}\nstart_condition:\n  command: [/bin/sh, -c, 'test -e \"$CONDITION\"']\n  timeout_seconds: 2\n  backoff:\n    initial_seconds: 1\n    max_seconds: 1\n    multiplier: 2\n    jitter_percent: 0\n"
        ),
    )?;
    Ok(())
}

fn prepare_broken_supervisor(definitions: &Path, runtime: &Path) -> Result<(), Box<dyn Error>> {
    fs::write(
        definitions.join("broken.yml"),
        "version: 2\nenabled: false\ncommand: [/bin/true]\n",
    )?;
    let applied = parse_str("version: 2\ncommand: [/bin/true]\n")?;
    DefinitionSnapshots::open(runtime)?.record_applied("broken", &applied)?;
    let service = runtime.join("broken");
    fs::create_dir(&service)?;
    fs::set_permissions(service, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn bind_broken_control(runtime: &Path) -> Result<StdUnixListener, Box<dyn Error>> {
    let socket = runtime.join("broken/immortal.sock");
    let listener = StdUnixListener::bind(&socket)?;
    fs::set_permissions(socket, fs::Permissions::from_mode(0o600))?;
    listener.set_nonblocking(true)?;
    Ok(listener)
}

async fn reject_control_clients(
    listener: StdUnixListener,
    attempts: mpsc::Sender<()>,
) -> io::Result<()> {
    let listener = tokio::net::UnixListener::from_std(listener)?;
    loop {
        let (stream, _address) = listener.accept().await?;
        attempts.send(()).await.map_err(io::Error::other)?;
        drop(stream);
    }
}

async fn exercise_reconciliation(
    definition: &Path,
    runtime: &Path,
    failed_attempts: &mut mpsc::Receiver<()>,
    locked_owner: RuntimeOwner,
) -> Result<(), Box<dyn Error>> {
    wait_for_ready_command(runtime, "api", "30").await?;
    wait_for_ready_command(runtime, "parallel-a", "-c").await?;
    wait_for_ready_command(runtime, "parallel-b", "-c").await?;
    wait_for_service_state(
        runtime,
        "conditioned",
        immortal_core::status::ServiceState::WaitingCondition,
    )
    .await?;
    let condition_status = service_status(runtime, "conditioned").await?.1;
    if condition_status
        .status
        .as_ref()
        .is_none_or(|status| status.starts != 0)
    {
        return Err("failed start condition consumed a service start".into());
    }
    let condition_marker = definition
        .parent()
        .and_then(Path::parent)
        .ok_or("api definition has no test root")?
        .join(".condition-ready");
    fs::write(condition_marker, b"")?;
    wait_for_ready_command(runtime, "conditioned", "30").await?;
    wait_for_ready_command(runtime, "dependent", "30").await?;
    if discover(runtime)?.services.contains_key("locked")
        || !supervisor_is_active(&runtime.join("locked"))?
    {
        return Err("active lock without control was not deferred".into());
    }
    drop(locked_owner);
    wait_for_ready_command(runtime, "locked", "30").await?;
    set_desired_state(runtime, "parallel-a", Operation::Stop).await?;
    wait_for_service_state(
        runtime,
        "parallel-a",
        immortal_core::status::ServiceState::Down,
    )
    .await?;
    wait_for_service_state(
        runtime,
        "dependent",
        immortal_core::status::ServiceState::Ready,
    )
    .await?;
    set_desired_state(runtime, "parallel-a", Operation::Start).await?;
    wait_for_ready_command(runtime, "parallel-a", "-c").await?;
    let initial_status = service_status(runtime, "api").await?.1;
    let initial = initial_status.generation;
    fs::write(definition, "version: 2\ncommand: [/bin/sleep, '30']\n")?;
    wait_for_reconciliation_window().await;
    if service_status(runtime, "api").await?.1.generation != initial {
        return Err("unchanged service was restarted".into());
    }
    set_desired_state(runtime, "api", Operation::Halt).await?;
    wait_for_service_absence(runtime, "api").await?;
    wait_for_ready_command(runtime, "api", "30").await?;

    fs::write(
        definition,
        "version: 2\nenabled: false\ncommand: [/bin/sleep, '30']\n",
    )?;
    wait_for_service_state(runtime, "api", immortal_core::status::ServiceState::Down).await?;

    fs::write(definition, "version: 2\ncommand: [/bin/sleep, '29']\n")?;
    wait_for_ready_command(runtime, "api", "29").await?;
    set_desired_state(runtime, "api", Operation::Stop).await?;
    wait_for_service_state(runtime, "api", immortal_core::status::ServiceState::Down).await?;

    fs::write(definition, "version: 2\ncommand: [/bin/sleep, '28']\n")?;
    wait_for_reconciliation_window().await;
    let status = service_status(runtime, "api").await?.1;
    if status
        .status
        .as_ref()
        .is_none_or(|status| status.state != immortal_core::status::ServiceState::Down)
    {
        return Err("changed service did not preserve Down state".into());
    }
    set_desired_state(runtime, "api", Operation::Start).await?;
    wait_for_ready_command(runtime, "api", "28").await?;

    fs::remove_file(definition)?;
    let definitions = definition
        .parent()
        .ok_or("api definition has no parent directory")?;
    fs::remove_file(definitions.join("parallel-a.yml"))?;
    fs::remove_file(definitions.join("parallel-b.yml"))?;
    fs::remove_file(definitions.join("dependent.yml"))?;
    fs::remove_file(definitions.join("launch-failure.yml"))?;
    fs::remove_file(definitions.join("locked.yml"))?;
    fs::remove_file(definitions.join("conditioned.yml"))?;
    wait_for_service_absence(runtime, "api").await?;
    wait_for_service_absence(runtime, "parallel-a").await?;
    wait_for_service_absence(runtime, "parallel-b").await?;
    wait_for_service_absence(runtime, "dependent").await?;
    wait_for_service_absence(runtime, "locked").await?;
    wait_for_service_absence(runtime, "conditioned").await?;
    wait_for_failed_retries(failed_attempts).await?;
    Ok(())
}

async fn wait_for_failed_retries(attempts: &mut mpsc::Receiver<()>) -> Result<(), Box<dyn Error>> {
    for _attempt in 0..2 {
        if timeout(DEADLINE, attempts.recv()).await?.is_none() {
            return Err("broken service retry channel closed early".into());
        }
    }
    Ok(())
}

async fn wait_for_ready_command(
    runtime: &Path,
    name: &str,
    argument: &str,
) -> Result<(), Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Ok((_service, response)) = service_status(runtime, name).await
            && response.status.as_ref().is_some_and(|status| {
                status.state == immortal_core::status::ServiceState::Ready
                    && status.command.get(1).map(String::as_str) == Some(argument)
            })
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("service `{name}` did not become Ready with `{argument}`").into());
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_reconciliation_window() {
    // One full periodic interval plus debounce proves that no mutation reflects
    // a completed authoritative scan rather than watcher latency.
    sleep(Duration::from_millis(1_500)).await;
}

async fn service_status(
    runtime: &Path,
    name: &str,
) -> Result<
    (
        immortal_core::runtime::RuntimeService,
        immortal_core::control::Response,
    ),
    Box<dyn Error>,
> {
    let service = discover(runtime)?
        .services
        .get(name)
        .cloned()
        .ok_or("requested supervisor is absent")?;
    let status = exchange(
        &service.socket,
        &Request {
            operation: Operation::Status,
            service: service.name.clone(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        },
    )
    .await?;
    Ok((service, status))
}

async fn set_desired_state(
    runtime: &Path,
    name: &str,
    operation: Operation,
) -> Result<(), Box<dyn Error>> {
    let (service, status) = service_status(runtime, name).await?;
    let response = exchange(
        &service.socket,
        &Request {
            operation,
            service: service.name,
            expected_generation: status
                .generation
                .map_or(GenerationMatch::NoChild, GenerationMatch::Exact),
            scope: SignalScope::Group,
            signal: None,
        },
    )
    .await?;
    if !response.code.is_success() {
        return Err(format!("{operation:?} was rejected").into());
    }
    Ok(())
}

async fn halt_if_present(runtime: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    if !discover(runtime)?.services.contains_key(name) {
        return Ok(());
    }
    set_desired_state(runtime, name, Operation::Halt).await?;
    wait_for_service_absence(runtime, name).await
}

async fn halt_services(runtime: &Path) -> Result<(), Box<dyn Error>> {
    for name in [
        "api",
        "parallel-a",
        "parallel-b",
        "dependent",
        "locked",
        "conditioned",
    ] {
        halt_if_present(runtime, name).await?;
    }
    Ok(())
}

async fn wait_for_service_state(
    runtime: &Path,
    name: &str,
    expected: immortal_core::status::ServiceState,
) -> Result<(), Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if let Ok((_service, response)) = service_status(runtime, name).await
            && response
                .status
                .as_ref()
                .is_some_and(|status| status.state == expected)
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("service `{name}` did not reach {expected:?}").into());
        }
        sleep(Duration::from_millis(20)).await;
    }
}

async fn wait_for_service_absence(runtime: &Path, name: &str) -> Result<(), Box<dyn Error>> {
    let deadline = tokio::time::Instant::now() + DEADLINE;
    loop {
        if !discover(runtime)?.services.contains_key(name)
            && !immortal_core::runtime::supervisor_is_active(&runtime.join(name))?
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!("service `{name}` did not disappear").into());
        }
        sleep(Duration::from_millis(20)).await;
    }
}

fn run_fake_immortal(arguments: &[String]) -> Result<(), Box<dyn Error>> {
    let config_path = arguments.get(2).ok_or("missing fake config path")?;
    if arguments.get(3).map(String::as_str) != Some("--control-dir") {
        return Err("missing fake control-dir option".into());
    }
    let control = arguments.get(4).ok_or("missing fake control directory")?;
    if Path::new(control)
        .file_name()
        .and_then(|name| name.to_str())
        == Some("launch-failure")
    {
        return Err("injected supervisor launcher failure".into());
    }
    prove_parallel_launcher(Path::new(control))?;
    prove_dependency_gate(Path::new(control))?;
    let config = parse_file(Path::new(config_path))?;
    match run_daemon(&config, Some(Path::new(control)))? {
        DaemonRunOutcome::Parent | DaemonRunOutcome::Daemon(_) => Ok(()),
    }
}

fn prove_dependency_gate(control: &Path) -> Result<(), Box<dyn Error>> {
    if control.file_name().and_then(|name| name.to_str()) != Some("dependent") {
        return Ok(());
    }
    let root = control
        .parent()
        .and_then(Path::parent)
        .ok_or("dependent control directory has no test root")?;
    if ["parallel-a", "parallel-b"]
        .into_iter()
        .all(|name| root.join(format!(".{name}-service")).exists())
    {
        Ok(())
    } else {
        Err("dependent launcher ran before its requirements started".into())
    }
}

fn prove_parallel_launcher(control: &Path) -> Result<(), Box<dyn Error>> {
    let Some(name) = control.file_name().and_then(|name| name.to_str()) else {
        return Err("fake control directory has no UTF-8 service name".into());
    };
    let sibling = match name {
        "parallel-a" => "parallel-b",
        "parallel-b" => "parallel-a",
        _ => return Ok(()),
    };
    let root = control
        .parent()
        .and_then(Path::parent)
        .ok_or("fake control directory has no test root")?;
    fs::write(root.join(format!(".{name}-launching")), b"")?;
    let sibling_marker = root.join(format!(".{sibling}-launching"));
    let deadline = Instant::now() + DEADLINE;
    while !sibling_marker.exists() {
        if Instant::now() >= deadline {
            return Err("independent supervisor launchers did not overlap".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

struct TestRoot(PathBuf);

impl TestRoot {
    fn new() -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!(
            "immortaldir-operational-contract-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
