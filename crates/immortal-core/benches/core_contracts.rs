use std::{
    error::Error,
    hint::black_box,
    time::{Duration, Instant},
};

use immortal_core::config::parse_str;
use immortal_core::control::{
    GenerationMatch, Operation, Request, Response, ResponseCode, SignalScope,
};
use immortal_core::status::StatusSnapshot;
use immortal_core::supervisor::StateMachine;

const SAMPLES: usize = 9;
const CONFIG_ITERATIONS: u64 = 1_000;
const REQUEST_ITERATIONS: u64 = 25_000;
const STATUS_ITERATIONS: u64 = 10_000;

const CONFIG: &str = r"
version: 2
command: [/usr/local/bin/api, --foreground]
environment:
  RUST_LOG: info
restart:
  policy: on-failure
  limits:
    max_retries: 100
    burst:
      starts: 10
      window_seconds: 60
  backoff:
    initial_seconds: 1
    max_seconds: 60
    multiplier: 2
    jitter_percent: 20
    reset_after_seconds: 60
readiness:
  mode: notify-fd
  timeout_seconds: 30
logging:
  combine_stderr: true
  stdout:
    logger: [/usr/bin/logger, -t, api]
";

fn main() -> Result<(), Box<dyn Error>> {
    benchmark("config-v2-parse", CONFIG_ITERATIONS, || {
        black_box(parse_str(black_box(CONFIG))?);
        Ok(())
    })?;

    let request = Request {
        operation: Operation::Status,
        service: "api.worker-1".to_owned(),
        expected_generation: GenerationMatch::Any,
        scope: SignalScope::Main,
        signal: None,
    };
    benchmark("control-request-roundtrip", REQUEST_ITERATIONS, || {
        let encoded = black_box(&request).encode()?;
        black_box(Request::decode(black_box(&encoded))?);
        Ok(())
    })?;

    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.supervisor_pid = Some(101);
    status.starts = 9;
    status.command = vec!["/usr/local/bin/api".to_owned(), "--foreground".to_owned()];
    let response = Response {
        code: ResponseCode::Ok,
        generation: None,
        message: "status".to_owned(),
        status: Some(status),
    };
    benchmark("control-status-roundtrip", STATUS_ITERATIONS, || {
        let encoded = black_box(&response).encode()?;
        black_box(Response::decode(black_box(&encoded))?);
        Ok(())
    })?;
    Ok(())
}

fn benchmark(
    name: &str,
    iterations: u64,
    mut operation: impl FnMut() -> Result<(), Box<dyn Error>>,
) -> Result<(), Box<dyn Error>> {
    for _ in 0..iterations.min(100) {
        operation()?;
    }
    let mut samples = Vec::with_capacity(SAMPLES);
    for _ in 0..SAMPLES {
        let started = Instant::now();
        for _ in 0..iterations {
            operation()?;
        }
        samples.push(started.elapsed());
    }
    samples.sort_unstable();
    let median = samples
        .get(SAMPLES / 2)
        .copied()
        .ok_or("benchmark did not produce a median")?;
    report(name, iterations, median);
    Ok(())
}

fn report(name: &str, iterations: u64, elapsed: Duration) {
    let nanos_per_operation = elapsed.as_nanos() / u128::from(iterations);
    println!("{name}\t{nanos_per_operation} ns/op\tmedian of {SAMPLES} samples");
}
