//! Sustained resource-soak contract for the process-broker boundary.
//!
//! One single-threaded parent forks a real process broker, then drives it
//! through a long spawn/signal/reap restart storm while sampling the broker's
//! own footprint. Every cycle materializes one generation, kills its group, and
//! reaps the terminal child, exercising the fork, descriptor, process-group,
//! and reaping paths that a leak would accumulate on. Between cycles the broker
//! is at rest, so [`resource_sampler`] observes its resident memory, open
//! descriptors, and surviving children without racing live churn.
//!
//! The contract is a diagnostic: it fails loudly when an operation stalls past
//! its deadline, when resident memory or descriptors grow beyond a generous
//! absolute tolerance over the run, when the broker retains children at rest,
//! or when any child survives a clean shutdown. A short bounded run is the
//! default so the suite exercises the machinery on every platform; the
//! `IMMORTAL_SOAK_*` overrides scale it into a long campaign that also records
//! schema-1 evidence for retention.

#[path = "support/broker_guard.rs"]
mod broker_guard;
#[path = "support/resource_sampler.rs"]
mod resource_sampler;
#[path = "support/soak_evidence.rs"]
mod soak_evidence;
#[path = "support/soak_thresholds.rs"]
mod soak_thresholds;

use std::env;
use std::error::Error;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use immortal_core::process::{
    BrokerSignalScope, ProcessBrokerClient, ProcessBrokerEvent, ProcessCommand, ProcessId,
    ProcessSignal, reap_any_event, start_process_broker,
};
use immortal_core::supervisor::Generation;
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;
use crate::resource_sampler::ResourceSample;
use crate::soak_evidence::{EvidenceEnvironment, EvidenceRow, Outcome};

const SCENARIO: &str = "broker-restart-soak";

const DEFAULT_CYCLES: u64 = 64;
const DEFAULT_SAMPLE_INTERVAL: u64 = 8;
const DEFAULT_DURATION_SAMPLES: u64 = 64;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const REAP_TIMEOUT: Duration = Duration::from_secs(5);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

const ENV_CYCLES: &str = "IMMORTAL_SOAK_CYCLES";
const ENV_DURATION: &str = "IMMORTAL_SOAK_DURATION_SECONDS";
const ENV_SAMPLE_INTERVAL: &str = "IMMORTAL_SOAK_SAMPLE_INTERVAL";
const ENV_SAMPLE_SECONDS: &str = "IMMORTAL_SOAK_SAMPLE_SECONDS";
const ENV_EVIDENCE: &str = "IMMORTAL_SOAK_EVIDENCE";

fn main() -> Result<(), Box<dyn Error>> {
    let config = SoakConfig::from_env()?;
    let report = collect_soak(&config)?;
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

/// How long the restart storm runs.
enum SoakBudget {
    /// Run an exact number of restart cycles.
    Cycles(u64),
    /// Run until the wall-clock duration elapses.
    Duration(Duration),
}

/// Parsed soak parameters resolved once from the environment.
struct SoakConfig {
    budget: SoakBudget,
    sample_interval: u64,
    sample_period: Option<Duration>,
    evidence_path: Option<PathBuf>,
}

impl SoakConfig {
    /// Resolve the soak parameters, rejecting a malformed override.
    ///
    /// Cycle budgets sample every `sample_interval` cycles; duration budgets
    /// sample on a wall-clock period instead, so a long campaign keeps a
    /// bounded number of evidence rows regardless of how fast cycles run.
    fn from_env() -> Result<Self, Box<dyn Error>> {
        let sample_interval = parse_env_u64(ENV_SAMPLE_INTERVAL)?
            .unwrap_or(DEFAULT_SAMPLE_INTERVAL)
            .max(1);
        let (budget, sample_period) = match parse_env_u64(ENV_DURATION)? {
            Some(seconds) => {
                let seconds = seconds.max(1);
                (
                    SoakBudget::Duration(Duration::from_secs(seconds)),
                    Some(sample_period_for(seconds)?),
                )
            }
            None => (
                SoakBudget::Cycles(parse_env_u64(ENV_CYCLES)?.unwrap_or(DEFAULT_CYCLES).max(1)),
                None,
            ),
        };
        Ok(Self {
            budget,
            sample_interval,
            sample_period,
            evidence_path: env::var_os(ENV_EVIDENCE).map(PathBuf::from),
        })
    }
}

/// Choose the at-rest sampling period for a duration budget, in seconds.
///
/// An explicit `IMMORTAL_SOAK_SAMPLE_SECONDS` override wins; otherwise the
/// period targets [`DEFAULT_DURATION_SAMPLES`] samples across the run, floored
/// at one second so sampling never busy-loops.
fn sample_period_for(duration_seconds: u64) -> Result<Duration, Box<dyn Error>> {
    let seconds = match parse_env_u64(ENV_SAMPLE_SECONDS)? {
        Some(explicit) => explicit.max(1),
        None => (duration_seconds / DEFAULT_DURATION_SAMPLES).max(1),
    };
    Ok(Duration::from_secs(seconds))
}

/// One at-rest observation taken after a completed restart cycle.
struct CycleSample {
    cycle: u64,
    resources: ResourceSample,
}

/// The data gathered from one soak run, evaluated after the broker is reaped.
struct SoakReport {
    samples: Vec<CycleSample>,
    max_restart_latency: Duration,
    cleanup: Cleanup,
}

/// Whether shutdown left the parent with no owned children.
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

/// Fork the broker, drive the storm, reap it, and confirm no orphan remains.
fn collect_soak(config: &SoakConfig) -> Result<SoakReport, Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let broker = endpoint.process();
    let mut guard = BrokerGuard::new(broker, REAP_TIMEOUT, POLL_INTERVAL);
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    let mut report = runtime.block_on(async {
        let client = endpoint.connect()?;
        drive_soak(client, broker, config).await
    })?;
    drop(runtime);
    guard.wait()?;
    report.cleanup = verify_no_orphans();
    Ok(report)
}

/// Run the restart storm on the connected client and sample the broker at rest.
async fn drive_soak(
    mut client: ProcessBrokerClient,
    broker: ProcessId,
    config: &SoakConfig,
) -> Result<SoakReport, Box<dyn Error>> {
    require_event(
        &mut client,
        |event| matches!(event, ProcessBrokerEvent::Ready),
        "broker readiness",
    )
    .await?;

    let deadline = match &config.budget {
        SoakBudget::Duration(duration) => Some(Instant::now() + *duration),
        SoakBudget::Cycles(_) => None,
    };

    let mut samples = Vec::new();
    let mut max_restart_latency = Duration::ZERO;
    let mut last_sampled = Instant::now();
    let mut cycle: u64 = 0;
    loop {
        cycle += 1;
        let started = Instant::now();
        run_one_restart(&mut client, cycle).await?;
        max_restart_latency = max_restart_latency.max(started.elapsed());
        if due_for_sample(config, cycle, last_sampled) {
            samples.push(CycleSample {
                cycle,
                resources: resource_sampler::sample(broker),
            });
            last_sampled = Instant::now();
        }
        if soak_complete(&config.budget, cycle, deadline) {
            break;
        }
    }
    samples.push(CycleSample {
        cycle,
        resources: resource_sampler::sample(broker),
    });

    client.shutdown().await?;
    require_event(
        &mut client,
        |event| matches!(event, ProcessBrokerEvent::ShutdownComplete),
        "shutdown completion",
    )
    .await?;

    Ok(SoakReport {
        samples,
        max_restart_latency,
        cleanup: Cleanup::Clean,
    })
}

/// Whether an at-rest sample is due after the just-completed cycle.
///
/// The first cycle always samples to fix a baseline. Duration budgets then
/// sample once their wall-clock period elapses; cycle budgets sample every
/// `sample_interval` cycles.
fn due_for_sample(config: &SoakConfig, cycle: u64, last_sampled: Instant) -> bool {
    if cycle == 1 {
        return true;
    }
    match config.sample_period {
        Some(period) => last_sampled.elapsed() >= period,
        None => cycle.is_multiple_of(config.sample_interval),
    }
}

/// Whether the configured budget has been reached.
fn soak_complete(budget: &SoakBudget, cycle: u64, deadline: Option<Instant>) -> bool {
    match budget {
        SoakBudget::Cycles(total) => cycle >= *total,
        SoakBudget::Duration(_) => deadline.is_some_and(|limit| Instant::now() >= limit),
    }
}

/// Materialize one generation, kill its group, and reap the terminal child.
async fn run_one_restart(
    client: &mut ProcessBrokerClient,
    cycle: u64,
) -> Result<(), Box<dyn Error>> {
    let generation = Generation::new(cycle)
        .ok_or_else(|| io::Error::other("soak cycle exceeded the generation range"))?;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("30");
    client.spawn(generation, command, STARTUP_TIMEOUT).await?;
    require_event(
        client,
        |event| {
            matches!(
                event,
                ProcessBrokerEvent::Started { generation: g, .. } if *g == generation
            )
        },
        "generation start",
    )
    .await?;
    client
        .signal(generation, BrokerSignalScope::Group, ProcessSignal::Kill)
        .await?;
    require_event(
        client,
        |event| {
            matches!(
                event,
                ProcessBrokerEvent::SignalDelivered { generation: g } if *g == generation
            )
        },
        "signal delivery",
    )
    .await?;
    require_event(
        client,
        |event| {
            matches!(
                event,
                ProcessBrokerEvent::Child { generation: g, event: child }
                    if *g == generation && child.is_terminal()
            )
        },
        "terminal child event",
    )
    .await
}

/// Await the next broker event, failing loudly if it stalls or mismatches.
async fn require_event(
    client: &mut ProcessBrokerClient,
    predicate: impl FnOnce(&ProcessBrokerEvent) -> bool,
    description: &str,
) -> Result<(), Box<dyn Error>> {
    let event = match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await {
        Ok(result) => result?,
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("broker did not deliver {description} within the event deadline"),
            )
            .into());
        }
    };
    if predicate(&event) {
        Ok(())
    } else {
        Err(io::Error::other(format!("expected {description}, observed {event:?}")).into())
    }
}

/// Drain the parent's remaining children, expecting `ECHILD` after shutdown.
fn verify_no_orphans() -> Cleanup {
    let deadline = Instant::now() + CLEANUP_TIMEOUT;
    loop {
        match reap_any_event() {
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => return Cleanup::Clean,
            Ok(Some(event)) => {
                return Cleanup::Leftover(format!("unexpected surviving child {event:?}"));
            }
            Ok(None) => {}
            Err(error) => return Cleanup::Leftover(format!("wait error after shutdown: {error}")),
        }
        if Instant::now() >= deadline {
            return Cleanup::Leftover("children remained after broker shutdown".to_owned());
        }
        thread::sleep(POLL_INTERVAL);
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

    let rss_baseline = report.samples.iter().find_map(|s| s.resources.resident_kib);
    let fd_baseline = report
        .samples
        .iter()
        .find_map(|s| s.resources.open_descriptors);

    for (index, sample) in report.samples.iter().enumerate() {
        let ordinal = u64::try_from(index).unwrap_or(u64::MAX).saturating_add(1);
        if let Some(rss) = sample.resources.resident_kib {
            let within = soak_thresholds::resident_within(rss_baseline, rss);
            rows.push(row(ordinal, "resident_kib", rss, "kibibytes", within));
            if !within {
                problems.push(format!(
                    "resident memory grew to {rss} KiB by cycle {}",
                    sample.cycle
                ));
            }
        }
        if let Some(fds) = sample.resources.open_descriptors {
            let within = soak_thresholds::descriptors_within(fd_baseline, fds);
            rows.push(row(ordinal, "open_descriptors", fds, "descriptors", within));
            if !within {
                problems.push(format!(
                    "open descriptors grew to {fds} by cycle {}",
                    sample.cycle
                ));
            }
        }
        if let Some(children) = sample.resources.child_processes {
            let within = soak_thresholds::children_within(children);
            rows.push(row(
                ordinal,
                "child_processes",
                children,
                "processes",
                within,
            ));
            if !within {
                problems.push(format!(
                    "broker retained {children} children at cycle {}",
                    sample.cycle
                ));
            }
        }
    }

    let latency_ms = duration_millis(report.max_restart_latency);
    let latency_within = soak_thresholds::latency_within(report.max_restart_latency);
    let latency_sample = u64::try_from(report.samples.len()).unwrap_or(1).max(1);
    rows.push(row(
        latency_sample,
        "restart_latency_ms",
        latency_ms,
        "milliseconds",
        latency_within,
    ));
    if !latency_within {
        problems.push(format!("peak restart latency reached {latency_ms} ms"));
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

/// Saturate a duration to whole milliseconds for the evidence schema.
fn duration_millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
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
