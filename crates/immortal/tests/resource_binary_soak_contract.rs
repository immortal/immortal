//! Sustained resource-soak contract for the full `immortal` supervisor binary.
//!
//! One parent process launches a real `immortal --foreground` supervisor against
//! a throwaway service whose child exits quickly under `restart: always`, so the
//! supervisor drives a continuous fork, exec, and reap restart storm. While the
//! storm runs, the parent periodically samples the supervisor process's own
//! resident memory, open descriptors, and surviving children, then tears the
//! supervisor down with a graceful signal and confirms it exited cleanly and
//! removed its control socket.
//!
//! # Flow
//!
//! provision runtime directory and config -> spawn supervisor -> wait for the
//! control socket -> sample the supervisor at wall-clock intervals -> graceful
//! terminate -> confirm clean exit and no leftover socket.
//!
//! The contract is a diagnostic. It fails loudly when the supervisor will not
//! start, when its footprint grows past a generous absolute tolerance over the
//! run, when it retains children, when it exits mid-soak, or when it does not
//! shut down cleanly. A short bounded run is the default so the suite exercises
//! the full binary on every platform; the `IMMORTAL_BINSOAK_*` overrides scale
//! it into a long campaign that records schema-1 evidence for retention.
//!
//! This complements the broker-boundary `resource_soak_contract` in
//! `immortal-core`: that instrument samples the forked broker directly, while
//! this one samples the whole supervisor process end to end through its
//! installed binary.

#[path = "../../immortal-core/tests/support/resource_sampler.rs"]
mod resource_sampler;
#[path = "../../immortal-core/tests/support/soak_evidence.rs"]
mod soak_evidence;
// This full-binary soak samples footprint only; the shared thresholds module
// also carries the broker contract's restart-latency predicate, which is unused
// here, so its dead code is tolerated for this one included support module.
#[allow(dead_code)]
#[path = "../../immortal-core/tests/support/soak_thresholds.rs"]
mod soak_thresholds;

use std::env;
use std::error::Error;
use std::fs::{self, File};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use immortal_core::process::{ProcessId, ProcessSignal, SignalTarget, signal as deliver_signal};

use crate::resource_sampler::ResourceSample;
use crate::soak_evidence::{EvidenceEnvironment, EvidenceRow, Outcome};

const SCENARIO: &str = "supervisor-restart-soak";

const STARTUP_TIMEOUT: Duration = Duration::from_secs(10);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const DIAGNOSTIC_LIMIT: u64 = 16 * 1024;

const DEFAULT_DURATION: Duration = Duration::from_secs(3);
const DEFAULT_SAMPLE_INTERVAL: Duration = Duration::from_millis(250);
const MIN_SAMPLE_INTERVAL: Duration = Duration::from_millis(10);

const ENV_DURATION: &str = "IMMORTAL_BINSOAK_DURATION_SECONDS";
const ENV_SAMPLE_MILLIS: &str = "IMMORTAL_BINSOAK_SAMPLE_MILLIS";
const ENV_EVIDENCE: &str = "IMMORTAL_BINSOAK_EVIDENCE";

static RUNTIME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn main() -> Result<(), Box<dyn Error>> {
    let binary = Path::new(env!("CARGO_BIN_EXE_immortal"));
    let config = SoakConfig::from_env()?;
    let report = collect_soak(binary, &config)?;
    let assessment = assess(&report);
    if let Some(path) = &config.evidence_path {
        let environment = EvidenceEnvironment::detect();
        soak_evidence::write_results(
            path,
            &environment,
            SCENARIO,
            assessment.cleanup_label,
            &assessment.rows,
        )?;
    }
    if assessment.problems.is_empty() {
        Ok(())
    } else {
        Err(io::Error::other(assessment.problems.join("; ")).into())
    }
}

/// Parsed soak parameters resolved once from the environment.
struct SoakConfig {
    duration: Duration,
    sample_interval: Duration,
    evidence_path: Option<PathBuf>,
}

impl SoakConfig {
    /// Resolve the soak parameters, rejecting a malformed override.
    ///
    /// The defaults keep the continuous-integration run short and deterministic;
    /// the duration and sample-period overrides scale it into a long campaign.
    fn from_env() -> Result<Self, Box<dyn Error>> {
        let duration = match parse_env_u64(ENV_DURATION)? {
            Some(seconds) => Duration::from_secs(seconds.max(1)),
            None => DEFAULT_DURATION,
        };
        let sample_interval = match parse_env_u64(ENV_SAMPLE_MILLIS)? {
            Some(millis) => Duration::from_millis(millis).max(MIN_SAMPLE_INTERVAL),
            None => DEFAULT_SAMPLE_INTERVAL,
        };
        Ok(Self {
            duration,
            sample_interval,
            evidence_path: env::var_os(ENV_EVIDENCE).map(PathBuf::from),
        })
    }
}

/// The data gathered from one soak run, evaluated after the supervisor exits.
struct SoakReport {
    samples: Vec<ResourceSample>,
    cleanup: Cleanup,
}

/// Whether the supervisor shut down cleanly and left no owned runtime state.
enum Cleanup {
    Clean,
    Leftover(String),
}

impl Cleanup {
    /// Render the cleanup result as the evidence schema's non-empty token.
    const fn label(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Leftover(_) => "leftover-after-shutdown",
        }
    }
}

/// Launch the supervisor, sample it under the storm, and tear it down cleanly.
fn collect_soak(binary: &Path, config: &SoakConfig) -> Result<SoakReport, Box<dyn Error>> {
    let runtime = RuntimeDirectory::new()?;
    let config_file = ConfigFile::new("binsoak", &storm_config())?;
    let child = spawn_supervisor(binary, &config_file, &runtime)?;
    let mut guard = SupervisorGuard::new(child);
    runtime.wait_for_socket(STARTUP_TIMEOUT)?;
    let supervisor = guard.process()?;
    let samples = sample_footprint(supervisor, config, &mut guard)?;
    let cleanup = shutdown_and_verify(&mut guard, &runtime);
    Ok(SoakReport { samples, cleanup })
}

/// Sample the supervisor at each wall-clock interval until the budget elapses.
///
/// Fails when the supervisor exits during the storm, which means it stopped
/// supervising rather than merely restarting its child.
fn sample_footprint(
    supervisor: ProcessId,
    config: &SoakConfig,
    guard: &mut SupervisorGuard,
) -> Result<Vec<ResourceSample>, Box<dyn Error>> {
    let start = Instant::now();
    let mut samples = Vec::new();
    let mut next = start;
    loop {
        let now = Instant::now();
        if now >= next {
            guard.ensure_running()?;
            samples.push(resource_sampler::sample(supervisor));
            next = now + config.sample_interval;
        }
        if now.duration_since(start) >= config.duration {
            break;
        }
        thread::sleep(POLL_INTERVAL);
    }
    guard.ensure_running()?;
    samples.push(resource_sampler::sample(supervisor));
    Ok(samples)
}

/// Terminate the supervisor and classify whether teardown was clean.
///
/// A graceful signal must produce a successful exit within the deadline and
/// remove the control socket; a required kill, a non-success exit, or a
/// surviving socket is recorded as leftover so the evidence and the failure
/// both surface it.
fn shutdown_and_verify(guard: &mut SupervisorGuard, runtime: &RuntimeDirectory) -> Cleanup {
    guard.terminate();
    match guard.wait(SHUTDOWN_TIMEOUT) {
        Ok(status) if status.success() => {
            if runtime.socket().exists() {
                Cleanup::Leftover("control socket remained after shutdown".to_owned())
            } else {
                Cleanup::Clean
            }
        }
        Ok(status) => Cleanup::Leftover(format!(
            "supervisor exited with {status} after graceful termination"
        )),
        Err(error) => Cleanup::Leftover(format!(
            "supervisor required a kill after graceful termination: {error}"
        )),
    }
}

/// The evaluated evidence rows and any bound violations from one soak run.
struct Assessment {
    rows: Vec<EvidenceRow>,
    problems: Vec<String>,
    cleanup_label: &'static str,
}

/// Turn samples into evidence rows and collect every bound violation.
fn assess(report: &SoakReport) -> Assessment {
    let mut rows = Vec::new();
    let mut problems = Vec::new();

    let rss_baseline = report.samples.iter().find_map(|sample| sample.resident_kib);
    let fd_baseline = report
        .samples
        .iter()
        .find_map(|sample| sample.open_descriptors);

    for (index, sample) in report.samples.iter().enumerate() {
        let ordinal = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        if let Some(rss) = sample.resident_kib {
            let within = soak_thresholds::resident_within(rss_baseline, rss);
            rows.push(row(ordinal, "resident_kib", rss, "kibibytes", within));
            if !within {
                problems.push(format!("supervisor resident memory grew to {rss} KiB"));
            }
        }
        if let Some(fds) = sample.open_descriptors {
            let within = soak_thresholds::descriptors_within(fd_baseline, fds);
            rows.push(row(ordinal, "open_descriptors", fds, "descriptors", within));
            if !within {
                problems.push(format!("supervisor open descriptors grew to {fds}"));
            }
        }
        if let Some(children) = sample.child_processes {
            let within = soak_thresholds::children_within(children);
            rows.push(row(
                ordinal,
                "child_processes",
                children,
                "processes",
                within,
            ));
            if !within {
                problems.push(format!("supervisor retained {children} children"));
            }
        }
    }

    if let Cleanup::Leftover(detail) = &report.cleanup {
        problems.push(detail.clone());
    }

    Assessment {
        rows,
        problems,
        cleanup_label: report.cleanup.label(),
    }
}

/// Build one evidence row from a measured value and its bound outcome.
fn row(
    sample: u64,
    metric: &'static str,
    value: u64,
    unit: &'static str,
    pass: bool,
) -> EvidenceRow {
    EvidenceRow {
        sample,
        metric,
        value: value.to_string(),
        unit,
        outcome: if pass { Outcome::Pass } else { Outcome::Fail },
    }
}

/// The throwaway service definition that drives the restart storm.
///
/// A short-lived child under `restart: always` makes the supervisor fork, exec,
/// and reap continuously; the minimal one-second backoff keeps the churn steady
/// without spinning.
fn storm_config() -> String {
    concat!(
        "version: 2\n",
        "command: [/bin/sleep, \"1\"]\n",
        "restart:\n",
        "  policy: always\n",
        "  backoff:\n",
        "    initial_seconds: 1\n",
        "    max_seconds: 1\n",
        "    multiplier: 1\n",
        "    jitter_percent: 0\n",
        "    reset_after_seconds: 1\n",
    )
    .to_owned()
}

/// Spawn `immortal --foreground` against the throwaway service and control dir.
fn spawn_supervisor(
    binary: &Path,
    config: &ConfigFile,
    runtime: &RuntimeDirectory,
) -> Result<Child, Box<dyn Error>> {
    let diagnostics = File::create(runtime.diagnostics())?;
    Command::new(binary)
        .args([
            "--foreground",
            "--config",
            config.path_str()?,
            "--control-dir",
            path_str(runtime.service())?,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(diagnostics))
        .spawn()
        .map_err(Into::into)
}

/// A live `immortal` supervisor whose drop guarantees the process is reaped.
struct SupervisorGuard {
    child: Child,
    finished: bool,
}

impl SupervisorGuard {
    const fn new(child: Child) -> Self {
        Self {
            child,
            finished: false,
        }
    }

    /// The supervisor's process identity, validated as a positive PID.
    fn process(&self) -> Result<ProcessId, Box<dyn Error>> {
        ProcessId::new(i32::try_from(self.child.id())?)
            .ok_or_else(|| "supervisor PID must be positive".into())
    }

    /// Confirm the supervisor is still running, reaping and failing if it exited.
    fn ensure_running(&mut self) -> Result<(), Box<dyn Error>> {
        match self.child.try_wait()? {
            Some(status) => {
                self.finished = true;
                Err(format!("supervisor exited during the soak with {status}").into())
            }
            None => Ok(()),
        }
    }

    /// Request graceful shutdown; best-effort because the peer may already be gone.
    fn terminate(&self) {
        if let Ok(process) = self.process() {
            let _ = deliver_signal(SignalTarget::Process(process), ProcessSignal::Terminate);
        }
    }

    /// Wait for exit within the deadline, killing and reaping on timeout.
    fn wait(&mut self, timeout: Duration) -> io::Result<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait()? {
                self.finished = true;
                return Ok(status);
            }
            if Instant::now() >= deadline {
                self.child.kill()?;
                let _ = self.child.wait();
                self.finished = true;
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "supervisor did not exit after graceful termination",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for SupervisorGuard {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// A temporary runtime root whose drop removes every owned directory and socket.
struct RuntimeDirectory {
    root: PathBuf,
    service: PathBuf,
    socket: PathBuf,
    diagnostics: PathBuf,
}

impl RuntimeDirectory {
    fn new() -> io::Result<Self> {
        let root = Path::new("/tmp").join(format!(
            "immortal-binsoak-{}-{}",
            std::process::id(),
            RUNTIME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        let root = fs::canonicalize(root)?;
        let service = root.join("soak");
        let socket = service.join("immortal.sock");
        let diagnostics = root.join("immortal.stderr");
        Ok(Self {
            root,
            service,
            socket,
            diagnostics,
        })
    }

    fn service(&self) -> &Path {
        &self.service
    }

    fn socket(&self) -> &Path {
        &self.socket
    }

    fn diagnostics(&self) -> &Path {
        &self.diagnostics
    }

    /// Wait for the supervisor to publish its control socket, or report why not.
    fn wait_for_socket(&self, timeout: Duration) -> Result<(), Box<dyn Error>> {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.socket.exists() {
                return Ok(());
            }
            thread::sleep(POLL_INTERVAL);
        }
        let diagnostics = read_diagnostics(&self.diagnostics)
            .unwrap_or_else(|error| format!("diagnostics unavailable: {error}"));
        Err(format!(
            "supervisor control socket was not created within {timeout:?}; diagnostics: {diagnostics}"
        )
        .into())
    }
}

impl Drop for RuntimeDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Read a bounded prefix of the supervisor's captured diagnostics.
fn read_diagnostics(path: &Path) -> io::Result<String> {
    use std::io::Read as _;

    let mut bytes = Vec::new();
    File::open(path)?
        .take(DIAGNOSTIC_LIMIT)
        .read_to_end(&mut bytes)?;
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// A temporary configuration file removed on drop.
struct ConfigFile(PathBuf);

impl ConfigFile {
    fn new(name: &str, contents: &str) -> io::Result<Self> {
        let path = env::temp_dir().join(format!(
            "immortal-{name}-{}-{}.yml",
            std::process::id(),
            RUNTIME_SEQUENCE.fetch_add(1, Ordering::Relaxed)
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

/// Borrow one filesystem path as UTF-8, rejecting non-UTF-8 temporary paths.
fn path_str(path: &Path) -> Result<&str, Box<dyn Error>> {
    path.to_str()
        .ok_or_else(|| "temporary path is not UTF-8".into())
}

/// Parse one optional non-negative integer override, rejecting malformed input.
fn parse_env_u64(name: &str) -> Result<Option<u64>, Box<dyn Error>> {
    match env::var_os(name) {
        None => Ok(None),
        Some(value) => {
            let text = value
                .into_string()
                .map_err(|_| io::Error::other(format!("{name} is not valid UTF-8")))?;
            let parsed = text
                .trim()
                .parse::<u64>()
                .map_err(|_| io::Error::other(format!("{name} must be a non-negative integer")))?;
            Ok(Some(parsed))
        }
    }
}
