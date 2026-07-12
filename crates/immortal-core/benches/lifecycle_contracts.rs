//! Fork-backed lifecycle latency measurements with bounded process ownership.
//!
//! The harness starts its single-threaded broker before constructing Tokio,
//! measures complete request-to-reap paths, and retains enough process-group
//! identity to clean up a failed sample. It deliberately uses no shared state:
//! the harness owns the broker client and cleanup guard, and each async
//! operation borrows them only until that operation completes.
//!
//! Metrics use fixed stack arrays, so sample collection has constant memory and
//! no heap allocation. Each iteration creates one owned command and necessarily
//! copies its bounded protocol frame across the process boundary. A current-thread
//! Tokio runtime schedules only the benchmark future and broker socket reader;
//! there are no locks or worker-runtime migrations. Runtime is linear in
//! `samples * iterations`, with at most one measured child group alive at once.

use std::{
    error::Error,
    io, thread,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        BrokerSignalScope, ChildEvent, ProcessBrokerClient, ProcessBrokerEndpoint,
        ProcessBrokerEvent, ProcessCommand, ProcessGroupId, ProcessId, ProcessSignal, SignalTarget,
        reap_any_event, signal, start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
const SAMPLES: usize = 7;
const SPAWN_SIGNAL_ITERATIONS: u64 = 10;
const SPAWN_WAIT_ITERATIONS: u64 = 20;
const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const WARMUP_ITERATIONS: u64 = 3;

type BenchResult<T> = Result<T, Box<dyn Error>>;

fn main() -> BenchResult<()> {
    let endpoint = start_process_broker()?;
    let mut ownership = BrokerOwnership::new(endpoint.process());
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(run_benchmarks(endpoint, &mut ownership))?;
    drop(runtime);
    ownership.wait()?;
    Ok(())
}

async fn run_benchmarks(
    endpoint: ProcessBrokerEndpoint,
    ownership: &mut BrokerOwnership,
) -> BenchResult<()> {
    let mut client = endpoint.connect()?;
    expect_ready(next_event(&mut client).await?)?;
    let mut next_generation = 1;

    for _ in 0..WARMUP_ITERATIONS {
        spawn_and_wait(&mut client, ownership, &mut next_generation).await?;
        spawn_signal_and_wait(&mut client, ownership, &mut next_generation).await?;
    }

    let spawn_wait = measure_spawn_wait(&mut client, ownership, &mut next_generation).await?;
    report("fork-spawn-wait", SPAWN_WAIT_ITERATIONS, spawn_wait)?;

    let spawn_signal = measure_spawn_signal(&mut client, ownership, &mut next_generation).await?;
    report(
        "fork-spawn-signal-wait",
        SPAWN_SIGNAL_ITERATIONS,
        spawn_signal,
    )?;

    client.shutdown().await?;
    expect_shutdown(next_event(&mut client).await?)
}

async fn measure_spawn_wait(
    client: &mut ProcessBrokerClient,
    ownership: &mut BrokerOwnership,
    next_generation: &mut u64,
) -> BenchResult<[Duration; SAMPLES]> {
    let mut samples = [Duration::ZERO; SAMPLES];
    for sample in &mut samples {
        let started = Instant::now();
        for _ in 0..SPAWN_WAIT_ITERATIONS {
            spawn_and_wait(client, ownership, next_generation).await?;
        }
        *sample = started.elapsed();
    }
    Ok(samples)
}

async fn measure_spawn_signal(
    client: &mut ProcessBrokerClient,
    ownership: &mut BrokerOwnership,
    next_generation: &mut u64,
) -> BenchResult<[Duration; SAMPLES]> {
    let mut samples = [Duration::ZERO; SAMPLES];
    for sample in &mut samples {
        let started = Instant::now();
        for _ in 0..SPAWN_SIGNAL_ITERATIONS {
            spawn_signal_and_wait(client, ownership, next_generation).await?;
        }
        *sample = started.elapsed();
    }
    Ok(samples)
}

async fn spawn_and_wait(
    client: &mut ProcessBrokerClient,
    ownership: &mut BrokerOwnership,
    next_generation: &mut u64,
) -> BenchResult<()> {
    let generation = take_generation(next_generation)?;
    client
        .spawn(
            generation,
            ProcessCommand::new("/usr/bin/true"),
            STARTUP_TIMEOUT,
        )
        .await?;
    ownership.started(expect_started(next_event(client).await?, generation)?);
    expect_completion(next_event(client).await?, generation, ownership, false)
}

async fn spawn_signal_and_wait(
    client: &mut ProcessBrokerClient,
    ownership: &mut BrokerOwnership,
    next_generation: &mut u64,
) -> BenchResult<()> {
    let generation = take_generation(next_generation)?;
    let mut command = ProcessCommand::new("/bin/sleep");
    command.argument("30");
    client.spawn(generation, command, STARTUP_TIMEOUT).await?;
    ownership.started(expect_started(next_event(client).await?, generation)?);
    client
        .signal(
            generation,
            BrokerSignalScope::Group,
            ProcessSignal::Terminate,
        )
        .await?;
    expect_signal(next_event(client).await?, generation)?;
    expect_completion(next_event(client).await?, generation, ownership, true)
}

async fn next_event(client: &mut ProcessBrokerClient) -> BenchResult<ProcessBrokerEvent> {
    Ok(tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??)
}

fn take_generation(next: &mut u64) -> BenchResult<Generation> {
    let value = *next;
    *next = value
        .checked_add(1)
        .ok_or_else(|| io::Error::other("benchmark generation counter exhausted"))?;
    Generation::new(value)
        .ok_or_else(|| io::Error::other("benchmark generation must be nonzero"))
        .map_err(Into::into)
}

fn expect_ready(event: ProcessBrokerEvent) -> BenchResult<()> {
    match event {
        ProcessBrokerEvent::Ready => Ok(()),
        event => Err(unexpected("broker readiness", &event)),
    }
}

fn expect_started(event: ProcessBrokerEvent, expected: Generation) -> BenchResult<ProcessGroupId> {
    match event {
        ProcessBrokerEvent::Started {
            generation, group, ..
        } if generation == expected => Ok(group),
        event => Err(unexpected("generation start", &event)),
    }
}

fn expect_signal(event: ProcessBrokerEvent, expected: Generation) -> BenchResult<()> {
    match event {
        ProcessBrokerEvent::SignalDelivered { generation } if generation == expected => Ok(()),
        event => Err(unexpected("signal acknowledgement", &event)),
    }
}

fn expect_completion(
    event: ProcessBrokerEvent,
    expected: Generation,
    ownership: &mut BrokerOwnership,
    expect_signal: bool,
) -> BenchResult<()> {
    match event {
        ProcessBrokerEvent::Child { generation, event }
            if generation == expected && event.is_terminal() =>
        {
            ownership.completed();
            match (event, expect_signal) {
                (ChildEvent::Exited { code: 0, .. }, false)
                | (ChildEvent::Signaled { signal: 1.., .. }, true) => Ok(()),
                (event, _) => Err(io::Error::other(format!(
                    "generation completed unexpectedly: {event:?}"
                ))
                .into()),
            }
        }
        event => Err(unexpected("terminal child event", &event)),
    }
}

fn expect_shutdown(event: ProcessBrokerEvent) -> BenchResult<()> {
    match event {
        ProcessBrokerEvent::ShutdownComplete => Ok(()),
        event => Err(unexpected("broker shutdown", &event)),
    }
}

fn unexpected(expected: &str, event: &ProcessBrokerEvent) -> Box<dyn Error> {
    io::Error::other(format!("expected {expected}, received {event:?}")).into()
}

fn report(name: &str, iterations: u64, mut samples: [Duration; SAMPLES]) -> BenchResult<()> {
    samples.sort_unstable();
    let median = samples
        .get(SAMPLES / 2)
        .copied()
        .ok_or_else(|| io::Error::other("benchmark did not produce a median"))?;
    let nanos_per_operation = median.as_nanos() / u128::from(iterations);
    println!("{name}\t{nanos_per_operation} ns/op\tmedian of {SAMPLES} samples");
    Ok(())
}

struct BrokerOwnership {
    active_group: Option<ProcessGroupId>,
    process: ProcessId,
    reaped: bool,
}

impl BrokerOwnership {
    const fn new(process: ProcessId) -> Self {
        Self {
            active_group: None,
            process,
            reaped: false,
        }
    }

    fn started(&mut self, group: ProcessGroupId) {
        self.active_group = Some(group);
    }

    fn completed(&mut self) {
        self.active_group = None;
    }

    fn wait(&mut self) -> io::Result<()> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(event) = self.reap_once()? {
                return match event {
                    ChildEvent::Exited { code: 0, .. } => Ok(()),
                    event => Err(io::Error::other(format!(
                        "broker terminated unsuccessfully: {event:?}"
                    ))),
                };
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for benchmark broker",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn reap_once(&mut self) -> io::Result<Option<ChildEvent>> {
        match reap_any_event()? {
            Some(event) if event.pid() == self.process && event.is_terminal() => {
                self.reaped = true;
                Ok(Some(event))
            }
            Some(event) => Err(io::Error::other(format!(
                "benchmark reaped unexpected child event {event:?}"
            ))),
            None => Ok(None),
        }
    }
}

impl Drop for BrokerOwnership {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        if let Some(group) = self.active_group {
            let _ = signal(SignalTarget::Group(group), ProcessSignal::Kill);
        }
        let cleanup_deadline = Instant::now() + EVENT_TIMEOUT;
        while Instant::now() < cleanup_deadline {
            match self.reap_once() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Err(_) => break,
            }
        }
        let _ = signal(SignalTarget::Process(self.process), ProcessSignal::Kill);
        let reap_deadline = Instant::now() + EVENT_TIMEOUT;
        while Instant::now() < reap_deadline {
            match self.reap_once() {
                Ok(None) => thread::sleep(POLL_INTERVAL),
                Ok(Some(_)) | Err(_) => return,
            }
        }
    }
}
