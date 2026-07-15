//! Directory scans and operational reconciliation.
//!
//! Each trigger performs a complete scan, updates one owned desired-state
//! tracker, validates live runtime entries, then applies generation-bound
//! control mutations and checked launches. Launch snapshots, last-applied
//! state, and bounded deletion confirmations survive `immortaldir` restarts;
//! PID files never participate in identity. Tracker state is checkpointed
//! before mutations, and a confirmed deletion is acknowledged only after the
//! supervisor and applied snapshot are gone. The launcher broker is created by
//! this handler before Tokio and moved into this single-owner loop, so no shared
//! mutable lifecycle state is needed.
//! Service-local failures remain typed and pending while unrelated work
//! continues; loss of the runtime root or broker still fails the loop closed.
//! TERM or INT is observed while idle or during reconciliation. An in-flight
//! mutation still reaches its safe boundary before the launcher broker is shut
//! down and reaped.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    io::{self, Write},
    path::Path,
    time::Duration,
};

use immortal_core::{
    control::{
        GenerationMatch, Operation, Request, Response, ResponseCode, SignalScope, TransportError,
        exchange,
    },
    process::{ProcessBrokerEndpoint, start_process_broker},
    reconcile::{
        DefinitionSnapshots, DesiredStateTracker, LauncherError, ReconcileAction, ScanLimits,
        ScanResult, SupervisorLaunch, SupervisorLauncher, canonical_definitions_directory,
        dependency_plan, scan_directory,
    },
    runtime::{RuntimeRootError, RuntimeService, discover, supervisor_is_active},
    shutdown::TerminationSignals,
    status::ServiceState,
    watch::{DEFAULT_DEBOUNCE, ReconcileTriggers},
};
use tokio::{
    runtime::Builder,
    time::{sleep, timeout},
};

use super::{ActionError, ReconcileAction as DirectoryAction};

const LIFECYCLE_TIMEOUT: Duration = Duration::from_secs(15);
const LIFECYCLE_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// One isolated service mutation failure retained after a complete scan.
#[derive(Debug)]
pub struct ServiceFailure {
    service: String,
    kind: ServiceFailureKind,
}

impl ServiceFailure {
    fn new(service: &str, kind: ServiceFailureKind) -> Self {
        Self {
            service: service.to_owned(),
            kind,
        }
    }
}

impl Display for ServiceFailure {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "service `{}`: {}", self.service, self.kind)
    }
}

impl Error for ServiceFailure {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        self.kind.source()
    }
}

#[derive(Debug)]
enum ServiceFailureKind {
    Io(io::Error),
    Launcher(LauncherError),
    Transport(TransportError),
    Remote(ResponseCode),
    LifecycleTimeout,
    ActiveWithoutControl,
}

impl Display for ServiceFailureKind {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => Display::fmt(error, formatter),
            Self::Launcher(error) => Display::fmt(error, formatter),
            Self::Transport(error) => Display::fmt(error, formatter),
            Self::Remote(code) => write!(formatter, "mutation rejected: {}", code.name()),
            Self::LifecycleTimeout => formatter.write_str("lifecycle deadline exceeded"),
            Self::ActiveWithoutControl => {
                formatter.write_str("supervisor lock is active but control socket is unavailable")
            }
        }
    }
}

impl Error for ServiceFailureKind {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Launcher(error) => Some(error),
            Self::Transport(error) => Some(error),
            Self::Remote(_) | Self::LifecycleTimeout | Self::ActiveWithoutControl => None,
        }
    }
}

impl From<io::Error> for ServiceFailureKind {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<TransportError> for ServiceFailureKind {
    fn from(error: TransportError) -> Self {
        Self::Transport(error)
    }
}

enum MutationError {
    Isolated(ServiceFailureKind),
    RuntimeLost(RuntimeRootError),
}

impl From<ServiceFailureKind> for MutationError {
    fn from(error: ServiceFailureKind) -> Self {
        Self::Isolated(error)
    }
}

impl From<io::Error> for MutationError {
    fn from(error: io::Error) -> Self {
        Self::Isolated(error.into())
    }
}

/// Create process infrastructure in safe order and execute reconciliation.
///
/// # Errors
///
/// Returns an error when broker or runtime initialization fails, or when
/// reconciliation fails.
pub fn execute(action: &DirectoryAction) -> Result<(), ActionError> {
    let endpoint = if action.dry_run {
        None
    } else {
        Some(start_process_broker().map_err(ActionError::Broker)?)
    };
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(ActionError::RuntimeInitialization)?;
    runtime.block_on(execute_with_endpoint(action, endpoint))
}

/// Execute reconciliation with an already-created process broker endpoint.
///
/// # Errors
///
/// Returns an error when definitions/runtime state is unsafe, dependency
/// planning fails, a checked launch or generation-bound mutation fails, a
/// lifecycle deadline expires, watcher/output I/O fails, or operational mode
/// lacks its pre-Tokio broker.
pub async fn execute_with_endpoint(
    action: &DirectoryAction,
    endpoint: Option<ProcessBrokerEndpoint>,
) -> Result<(), ActionError> {
    let directory = canonical_definitions_directory(&action.directory)?;
    if action.dry_run {
        let mut tracker = DesiredStateTracker::default();
        if action.once {
            return scan_and_print(&directory, &mut tracker);
        }
        return watch_dry_run(action, &directory, &mut tracker).await;
    }

    let endpoint = endpoint.ok_or(ActionError::MissingBroker)?;
    let snapshots = DefinitionSnapshots::open(&action.runtime_directory)?;
    let mut tracker = snapshots.load_tracker()?;
    let mut launcher = SupervisorLauncher::connect(endpoint).await?;
    let mut operational = OperationalState::default();
    if action.once {
        let result = scan_and_apply(
            action,
            &directory,
            &snapshots,
            &mut tracker,
            &mut operational,
            &mut launcher,
        )
        .await;
        return finish_launcher(launcher, result).await;
    }

    let mut triggers = ReconcileTriggers::with_intervals(
        &directory,
        DEFAULT_DEBOUNCE,
        Duration::from_secs(action.scan_interval_seconds),
    )?;
    let mut signals = TerminationSignals::new()?;
    loop {
        let trigger = tokio::select! {
            result = signals.recv() => {
                return finish_launcher(launcher, result.map_err(ActionError::from)).await;
            }
            trigger = triggers.next() => trigger,
        };
        for error in trigger.watcher_errors {
            writeln!(io::stderr().lock(), "watcher: {error}")?;
        }
        let (reconciliation, termination) = {
            let reconciliation = scan_and_apply(
                action,
                &directory,
                &snapshots,
                &mut tracker,
                &mut operational,
                &mut launcher,
            );
            tokio::pin!(reconciliation);
            tokio::select! {
                result = &mut reconciliation => (result, None),
                termination = signals.recv() => {
                    (reconciliation.await, Some(termination))
                }
            }
        };
        match reconciliation {
            Ok(()) => {}
            Err(error @ (ActionError::Launcher(_) | ActionError::Runtime(_))) => {
                return finish_launcher(launcher, Err(error)).await;
            }
            Err(error) => writeln!(io::stderr().lock(), "reconcile: {error}")?,
        }
        if let Some(termination) = termination {
            return finish_launcher(launcher, termination.map_err(ActionError::from)).await;
        }
    }
}

async fn finish_launcher(
    launcher: SupervisorLauncher,
    result: Result<(), ActionError>,
) -> Result<(), ActionError> {
    let shutdown = launcher.shutdown().await.map_err(ActionError::from);
    match (result, shutdown) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

async fn watch_dry_run(
    action: &DirectoryAction,
    directory: &Path,
    tracker: &mut DesiredStateTracker,
) -> Result<(), ActionError> {
    let mut triggers = ReconcileTriggers::with_intervals(
        directory,
        DEFAULT_DEBOUNCE,
        Duration::from_secs(action.scan_interval_seconds),
    )?;
    let mut signals = TerminationSignals::new()?;
    loop {
        let trigger = tokio::select! {
            result = signals.recv() => {
                result?;
                return Ok(());
            }
            trigger = triggers.next() => trigger,
        };
        for error in trigger.watcher_errors {
            writeln!(io::stderr().lock(), "watcher: {error}")?;
        }
        scan_and_print(directory, tracker)?;
    }
}

#[derive(Default)]
struct OperationalState {
    known: BTreeMap<String, immortal_core::config::ServiceConfig>,
    pending: BTreeMap<String, ReconcileAction>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DesiredPlan {
    Adopt,
    Apply(ReconcileAction),
    Keep,
}

struct PreparedLaunch {
    name: String,
    launch: SupervisorLaunch,
}

#[derive(Default)]
struct StopOutcome {
    failures: Vec<ServiceFailure>,
    deleted: Vec<String>,
}

async fn scan_and_apply(
    action: &DirectoryAction,
    directory: &Path,
    snapshots: &DefinitionSnapshots,
    tracker: &mut DesiredStateTracker,
    operational: &mut OperationalState,
    launcher: &mut SupervisorLauncher,
) -> Result<(), ActionError> {
    let scan = scan_directory(directory, ScanLimits::default())?;
    for problem in &scan.problems {
        writeln!(
            io::stderr().lock(),
            "{}: {:?}",
            problem.path.display(),
            problem.kind
        )?;
    }
    for (name, planned) in tracker.apply(&scan) {
        if planned != ReconcileAction::Keep {
            operational.pending.insert(name, planned);
        }
    }
    snapshots.record_tracker(tracker)?;
    let desired = tracker.desired();
    let discovery = discover(&action.runtime_directory)?;
    report_discovery_problems(&discovery)?;
    let mut failures = Vec::new();

    for (name, config) in desired {
        if !operational.known.contains_key(name) {
            match snapshots.load_applied(name) {
                Ok(Some(applied)) => {
                    operational.known.insert(name.clone(), applied);
                }
                Ok(None) => {}
                Err(error) => {
                    failures.push(ServiceFailure::new(name, error.into()));
                    continue;
                }
            }
        }
        match desired_plan(
            discovery.services.contains_key(name),
            config,
            operational.known.get(name),
        ) {
            DesiredPlan::Apply(planned) => {
                operational.pending.insert(name.clone(), planned);
            }
            DesiredPlan::Adopt => match snapshots.record_applied(name, config) {
                Ok(()) => {
                    operational.known.insert(name.clone(), config.clone());
                    operational.pending.remove(name);
                }
                Err(error) => failures.push(ServiceFailure::new(name, error.into())),
            },
            DesiredPlan::Keep => {
                operational.pending.remove(name);
            }
        }
    }

    let plan = dependency_plan(desired)?;
    let stopped = apply_stops(action, desired, snapshots, operational, &discovery).await?;
    failures.extend(stopped.failures);
    if !stopped.deleted.is_empty() {
        for name in stopped.deleted {
            tracker.acknowledge_deletion(&name);
        }
        snapshots.record_tracker(tracker)?;
    }
    let desired = tracker.desired();
    failures.extend(
        apply_start_waves(
            action,
            desired,
            snapshots,
            operational,
            launcher,
            plan.waves,
        )
        .await?,
    );
    if failures.is_empty() {
        Ok(())
    } else {
        Err(ActionError::Partial(failures))
    }
}

async fn apply_start_waves(
    action: &DirectoryAction,
    desired: &BTreeMap<String, immortal_core::config::ServiceConfig>,
    snapshots: &DefinitionSnapshots,
    operational: &mut OperationalState,
    launcher: &mut SupervisorLauncher,
    waves: Vec<Vec<String>>,
) -> Result<Vec<ServiceFailure>, ActionError> {
    let mut failures = Vec::new();
    let mut discovery = discover(&action.runtime_directory)?;
    for wave in waves {
        for chunk in wave.chunks(action.launch_concurrency.get()) {
            let mut prepared = Vec::with_capacity(chunk.len());
            for name in chunk {
                let Some(config) = desired.get(name) else {
                    continue;
                };
                if !matches!(
                    operational.pending.get(name),
                    Some(ReconcileAction::Start | ReconcileAction::Restart)
                ) {
                    continue;
                }
                match dependencies_ready(config, &discovery).await {
                    Ok(true) => {}
                    Ok(false) => continue,
                    Err(error) => {
                        failures.push(ServiceFailure::new(name, error));
                        continue;
                    }
                }
                match operational
                    .prepare_start_or_restart(action, name, config, snapshots, &discovery)
                    .await
                {
                    Ok(Some(launch)) => prepared.push(PreparedLaunch {
                        name: name.clone(),
                        launch,
                    }),
                    Ok(None) => {}
                    Err(MutationError::Isolated(error)) => {
                        failures.push(ServiceFailure::new(name, error));
                    }
                    Err(MutationError::RuntimeLost(error)) => {
                        return Err(ActionError::Runtime(error));
                    }
                }
            }
            failures.extend(
                apply_launch_batch(action, desired, snapshots, operational, launcher, prepared)
                    .await?,
            );
            discovery = discover(&action.runtime_directory)?;
        }
    }
    Ok(failures)
}

async fn apply_launch_batch(
    action: &DirectoryAction,
    desired: &BTreeMap<String, immortal_core::config::ServiceConfig>,
    snapshots: &DefinitionSnapshots,
    operational: &mut OperationalState,
    launcher: &mut SupervisorLauncher,
    prepared: Vec<PreparedLaunch>,
) -> Result<Vec<ServiceFailure>, ActionError> {
    let mut names = Vec::with_capacity(prepared.len());
    let mut specifications = Vec::with_capacity(prepared.len());
    for prepared in prepared {
        names.push(prepared.name);
        specifications.push(prepared.launch);
    }
    let outcomes = launcher
        .launch_batch(
            &action.supervisor_binary,
            specifications,
            action.launch_concurrency,
        )
        .await
        .map_err(ActionError::Launcher)?;
    if names.len() != outcomes.len() {
        return Err(ActionError::Launcher(LauncherError::OperatingSystem(
            io::Error::other("launch batch outcome count mismatch"),
        )));
    }
    let mut failures = Vec::new();
    for (name, outcome) in names.into_iter().zip(outcomes) {
        let Some(config) = desired.get(&name) else {
            continue;
        };
        let result = match outcome {
            Ok(()) => {
                operational
                    .complete_start(action, &name, config, snapshots)
                    .await
            }
            Err(error) => Err(MutationError::Isolated(ServiceFailureKind::Launcher(
                LauncherError::from(error),
            ))),
        };
        match result {
            Ok(()) => {}
            Err(MutationError::Isolated(error)) => {
                failures.push(ServiceFailure::new(&name, error));
            }
            Err(MutationError::RuntimeLost(error)) => {
                return Err(ActionError::Runtime(error));
            }
        }
    }
    Ok(failures)
}

fn desired_plan(
    supervisor_present: bool,
    desired: &immortal_core::config::ServiceConfig,
    applied: Option<&immortal_core::config::ServiceConfig>,
) -> DesiredPlan {
    match (supervisor_present, applied) {
        (true, Some(previous)) if !desired.enabled && previous.enabled => {
            DesiredPlan::Apply(ReconcileAction::Stop)
        }
        (true, None) => DesiredPlan::Adopt,
        (false, _) if desired.enabled => DesiredPlan::Apply(ReconcileAction::Start),
        (true, Some(previous)) if previous != desired => {
            DesiredPlan::Apply(ReconcileAction::Restart)
        }
        (true, Some(_)) | (false, _) => DesiredPlan::Keep,
    }
}

fn report_discovery_problems(
    discovery: &immortal_core::runtime::DiscoveryResult,
) -> Result<(), ActionError> {
    let mut diagnostics = io::stderr().lock();
    for problem in &discovery.problems {
        writeln!(
            diagnostics,
            "{}: ignored runtime entry: {:?}",
            problem.path.display(),
            problem.kind
        )?;
    }
    Ok(())
}

async fn apply_stops(
    action: &DirectoryAction,
    desired: &BTreeMap<String, immortal_core::config::ServiceConfig>,
    snapshots: &DefinitionSnapshots,
    operational: &mut OperationalState,
    discovery: &immortal_core::runtime::DiscoveryResult,
) -> Result<StopOutcome, ActionError> {
    let names: Vec<String> = operational
        .pending
        .iter()
        .filter_map(|(name, pending)| (*pending == ReconcileAction::Stop).then_some(name.clone()))
        .collect();
    let mut outcome = StopOutcome::default();
    for name in names {
        let deleting = !desired.contains_key(&name);
        let result = apply_one_stop(action, desired, snapshots, &name, discovery).await;
        match result {
            Ok(()) => {
                if let Some(config) = desired.get(&name) {
                    operational.known.insert(name.clone(), config.clone());
                } else {
                    operational.known.remove(&name);
                }
                operational.pending.remove(&name);
                if deleting {
                    outcome.deleted.push(name);
                }
            }
            Err(MutationError::Isolated(error)) => {
                outcome.failures.push(ServiceFailure::new(&name, error));
            }
            Err(MutationError::RuntimeLost(error)) => {
                return Err(ActionError::Runtime(error));
            }
        }
    }
    Ok(outcome)
}

async fn apply_one_stop(
    action: &DirectoryAction,
    desired: &BTreeMap<String, immortal_core::config::ServiceConfig>,
    snapshots: &DefinitionSnapshots,
    name: &str,
    discovery: &immortal_core::runtime::DiscoveryResult,
) -> Result<(), MutationError> {
    let Some(service) = discovery.services.get(name) else {
        if !desired.contains_key(name) {
            snapshots.remove_applied(name)?;
        }
        return Ok(());
    };
    if let Some(config) = desired.get(name) {
        mutate(service, Operation::Stop).await?;
        wait_for_state(&action.runtime_directory, name, ServiceState::Down).await?;
        snapshots.record_applied(name, config)?;
    } else {
        mutate(service, Operation::Halt).await?;
        wait_for_absence(&action.runtime_directory, name).await?;
        snapshots.remove_applied(name)?;
    }
    Ok(())
}

async fn dependencies_ready(
    config: &immortal_core::config::ServiceConfig,
    discovery: &immortal_core::runtime::DiscoveryResult,
) -> Result<bool, ServiceFailureKind> {
    for dependency in &config.requires {
        let Some(service) = discovery.services.get(dependency) else {
            return Ok(false);
        };
        let response = status(service).await?;
        if response
            .status
            .as_ref()
            .is_none_or(|status| status.state != ServiceState::Ready)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

impl OperationalState {
    async fn prepare_start_or_restart(
        &mut self,
        action: &DirectoryAction,
        name: &str,
        config: &immortal_core::config::ServiceConfig,
        snapshots: &DefinitionSnapshots,
        discovery: &immortal_core::runtime::DiscoveryResult,
    ) -> Result<Option<SupervisorLaunch>, MutationError> {
        let runtime_directory = action.runtime_directory.join(name);
        if !discovery.services.contains_key(name) && supervisor_is_active(&runtime_directory)? {
            return Err(MutationError::Isolated(
                ServiceFailureKind::ActiveWithoutControl,
            ));
        }
        let pending = self.pending.get(name).copied();
        if let Some(service) = discovery.services.get(name)
            && pending == Some(ReconcileAction::Restart)
        {
            let current = status(service).await?;
            if current
                .status
                .as_ref()
                .is_some_and(|status| status.state == ServiceState::Down)
                && self.known.get(name).is_some_and(|known| known.enabled)
            {
                return Ok(None);
            }
            mutate(service, Operation::Halt).await?;
            wait_for_absence(&action.runtime_directory, name).await?;
        } else if let Some(service) = discovery.services.get(name) {
            if self.known.get(name) == Some(config) {
                self.pending.remove(name);
                return Ok(None);
            }
            mutate(service, Operation::Halt).await?;
            wait_for_absence(&action.runtime_directory, name).await?;
        }

        let mut supervisor_config = config.clone();
        supervisor_config.requires.clear();
        let snapshot = snapshots.publish(name, &supervisor_config)?;
        Ok(Some(SupervisorLaunch::new(snapshot, runtime_directory)))
    }

    async fn complete_start(
        &mut self,
        action: &DirectoryAction,
        name: &str,
        config: &immortal_core::config::ServiceConfig,
        snapshots: &DefinitionSnapshots,
    ) -> Result<(), MutationError> {
        wait_for_state(&action.runtime_directory, name, ServiceState::Ready).await?;
        self.pending.remove(name);
        self.known.insert(name.to_owned(), config.clone());
        snapshots.record_applied(name, config)?;
        writeln!(io::stdout().lock(), "APPLIED\t{name}")?;
        Ok(())
    }
}

async fn status(service: &RuntimeService) -> Result<Response, ServiceFailureKind> {
    let request = Request {
        operation: Operation::Status,
        service: service.name.clone(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    let response = exchange(&service.socket, &request).await?;
    require_success(response)
}

async fn mutate(
    service: &RuntimeService,
    operation: Operation,
) -> Result<Response, ServiceFailureKind> {
    let current = status(service).await?;
    let request = Request {
        operation,
        service: service.name.clone(),
        expected_generation: current
            .generation
            .map_or(GenerationMatch::NoChild, GenerationMatch::Exact),
        scope: SignalScope::Group,
        signal: None,
    };
    let response = exchange(&service.socket, &request).await?;
    require_success(response)
}

fn require_success(response: Response) -> Result<Response, ServiceFailureKind> {
    if response.code.is_success() {
        Ok(response)
    } else {
        Err(ServiceFailureKind::Remote(response.code))
    }
}

async fn wait_for_state(
    root: &Path,
    name: &str,
    expected: ServiceState,
) -> Result<(), MutationError> {
    timeout(LIFECYCLE_TIMEOUT, async {
        loop {
            let discovery = discover(root).map_err(MutationError::RuntimeLost)?;
            if let Some(service) = discovery.services.get(name)
                && status(service)
                    .await?
                    .status
                    .as_ref()
                    .is_some_and(|status| status.state == expected)
            {
                return Ok::<(), MutationError>(());
            }
            sleep(LIFECYCLE_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| MutationError::Isolated(ServiceFailureKind::LifecycleTimeout))?
}

async fn wait_for_absence(root: &Path, name: &str) -> Result<(), MutationError> {
    let directory = root.join(name);
    timeout(LIFECYCLE_TIMEOUT, async {
        loop {
            let discovery = discover(root).map_err(MutationError::RuntimeLost)?;
            let active = supervisor_is_active(&directory)
                .map_err(ServiceFailureKind::from)
                .map_err(MutationError::from)?;
            if !discovery.services.contains_key(name) && !active {
                return Ok::<(), MutationError>(());
            }
            sleep(LIFECYCLE_POLL_INTERVAL).await;
        }
    })
    .await
    .map_err(|_| MutationError::Isolated(ServiceFailureKind::LifecycleTimeout))?
}

fn scan_and_print(
    directory: &std::path::Path,
    tracker: &mut DesiredStateTracker,
) -> Result<(), ActionError> {
    let scan = scan_directory(directory, ScanLimits::default())?;
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    write_plan(tracker, scan, &mut stdout, &mut stderr)?;
    Ok(())
}

fn write_plan(
    tracker: &mut DesiredStateTracker,
    scan: ScanResult,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<()> {
    let actions = tracker.apply(&scan);
    for (name, action) in actions {
        let verb = match action {
            ReconcileAction::Start => "START",
            ReconcileAction::Restart => "RESTART",
            ReconcileAction::Stop => "STOP",
            ReconcileAction::Keep => "KEEP",
        };
        writeln!(stdout, "{verb}\t{name}")?;
    }
    for problem in scan.problems {
        writeln!(stderr, "{}: {:?}", problem.path.display(), problem.kind)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs, io,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use immortal_core::{
        config::ServiceConfig,
        exit::ExitClass,
        reconcile::{DesiredStateTracker, ReconcileAction, ScanLimits, scan_directory},
    };

    use super::{
        ActionError, DesiredPlan, DirectoryAction, ServiceFailure, ServiceFailureKind,
        desired_plan, execute_with_endpoint, write_plan,
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = Path::new("/tmp").join(format!(
                "immortaldir-action-{}-{sequence}",
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
            let _ignored = fs::remove_dir_all(&self.0);
        }
    }

    fn action(directory: &Path, dry_run: bool) -> DirectoryAction {
        DirectoryAction {
            directory: directory.to_owned(),
            runtime_directory: PathBuf::from("/unused"),
            scan_interval_seconds: 30,
            supervisor_binary: PathBuf::from("immortal"),
            launch_concurrency: immortal_core::reconcile::LaunchConcurrency::default(),
            once: true,
            dry_run,
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn one_shot_dry_run_scans_successfully() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        execute_with_endpoint(&action(directory.path(), true), None).await?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn mutation_mode_requires_a_pre_runtime_broker() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        assert!(matches!(
            execute_with_endpoint(&action(directory.path(), false), None).await,
            Err(ActionError::MissingBroker)
        ));
        Ok(())
    }

    #[test]
    fn partial_failures_use_a_bounded_diagnostic_and_partial_exit() {
        let failures = (0..17)
            .map(|sequence| {
                ServiceFailure::new(
                    &format!("service-{sequence}"),
                    ServiceFailureKind::Io(io::Error::other("injected")),
                )
            })
            .collect();
        let error = ActionError::Partial(failures);
        assert_eq!(error.exit_class(), ExitClass::PartialFailure);
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("17 service mutation(s) failed"));
        assert!(diagnostic.contains("1 additional failure(s) omitted"));
        assert!(!diagnostic.contains("service-16"));
    }

    #[test]
    fn plan_output_is_deterministic_and_keeps_diagnostics_separate() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(
            directory.path().join("broken.yml"),
            "version: 2\ncommand: []\n",
        )?;
        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut tracker = DesiredStateTracker::default();
        write_plan(&mut tracker, scan, &mut stdout, &mut stderr)?;

        assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");
        let diagnostics = String::from_utf8(stderr)?;
        assert!(diagnostics.contains("broken.yml"));
        Ok(())
    }

    #[test]
    fn repeated_plans_confirm_deletion_without_duplicate_stop() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::default();

        let mut stdout = Vec::new();
        write_plan(
            &mut tracker,
            scan_directory(directory.path(), ScanLimits::default())?,
            &mut stdout,
            &mut Vec::new(),
        )?;
        assert_eq!(String::from_utf8(stdout)?, "START\tapi\n");

        fs::remove_file(path)?;
        let mut first_missing = Vec::new();
        write_plan(
            &mut tracker,
            scan_directory(directory.path(), ScanLimits::default())?,
            &mut first_missing,
            &mut Vec::new(),
        )?;
        assert_eq!(String::from_utf8(first_missing)?, "KEEP\tapi\n");

        let mut confirmed = Vec::new();
        write_plan(
            &mut tracker,
            scan_directory(directory.path(), ScanLimits::default())?,
            &mut confirmed,
            &mut Vec::new(),
        )?;
        assert_eq!(String::from_utf8(confirmed)?, "STOP\tapi\n");

        let mut repeated = Vec::new();
        write_plan(
            &mut tracker,
            scan_directory(directory.path(), ScanLimits::default())?,
            &mut repeated,
            &mut Vec::new(),
        )?;
        assert!(repeated.is_empty());
        Ok(())
    }

    #[test]
    fn applied_snapshot_recovers_live_action_without_in_memory_history()
    -> Result<(), Box<dyn Error>> {
        let enabled = ServiceConfig::for_command(vec!["/bin/true".to_owned()])?;
        let changed = ServiceConfig::for_command(vec!["/bin/false".to_owned()])?;
        let mut disabled = enabled.clone();
        disabled.enabled = false;

        assert_eq!(desired_plan(true, &enabled, None), DesiredPlan::Adopt);
        assert_eq!(
            desired_plan(true, &enabled, Some(&enabled)),
            DesiredPlan::Keep
        );
        assert_eq!(
            desired_plan(true, &changed, Some(&enabled)),
            DesiredPlan::Apply(ReconcileAction::Restart)
        );
        assert_eq!(
            desired_plan(true, &disabled, Some(&enabled)),
            DesiredPlan::Apply(ReconcileAction::Stop)
        );
        assert_eq!(
            desired_plan(true, &enabled, Some(&disabled)),
            DesiredPlan::Apply(ReconcileAction::Restart)
        );
        assert_eq!(
            desired_plan(false, &enabled, Some(&enabled)),
            DesiredPlan::Apply(ReconcileAction::Start)
        );
        assert_eq!(
            desired_plan(false, &disabled, Some(&disabled)),
            DesiredPlan::Keep
        );
        Ok(())
    }
}
