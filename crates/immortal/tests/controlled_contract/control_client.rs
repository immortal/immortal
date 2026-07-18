//! Control-socket client and state-polling helpers for the controlled contract.
//!
//! These helpers drive one supervisor over its Unix control socket and block
//! until observed status matches an expectation or a bounded deadline elapses,
//! so scenarios express intent without repeating request framing or polling.

use std::{
    error::Error,
    fs,
    path::Path,
    process::ExitStatus,
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    control::{
        GenerationMatch, Operation, Request, Response, ResponseCode, SignalScope, read_response,
        write_request,
    },
    exit::ExitClass,
    status::{LoggerStatus, ServiceState},
    supervisor::Generation,
};
use tokio::{net::UnixStream, runtime::Builder};

use crate::POLL_INTERVAL;

pub(crate) fn status(socket: &Path) -> Result<Response, Box<dyn Error>> {
    request(
        socket,
        &Request {
            operation: Operation::Status,
            service: socket
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                .ok_or("invalid test service path")?
                .to_owned(),
            expected_generation: GenerationMatch::Any,
            scope: SignalScope::Main,
            signal: None,
        },
    )
}

pub(crate) fn lifecycle_request(
    socket: &Path,
    service: &str,
    operation: Operation,
    generation: Generation,
) -> Result<Response, Box<dyn Error>> {
    request(
        socket,
        &Request {
            operation,
            service: service.to_owned(),
            expected_generation: GenerationMatch::Exact(generation),
            scope: SignalScope::Main,
            signal: None,
        },
    )
}

pub(crate) fn request(socket: &Path, request: &Request) -> Result<Response, Box<dyn Error>> {
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut stream = UnixStream::connect(socket).await?;
        write_request(&mut stream, request).await?;
        read_response(&mut stream).await.map_err(Into::into)
    })
}

pub(crate) fn require_ok(response: &Response, operation: &str) -> Result<(), Box<dyn Error>> {
    if response.code == ResponseCode::Ok {
        Ok(())
    } else {
        Err(format!(
            "{operation} returned {}: {}",
            response.code.name(),
            response.message
        )
        .into())
    }
}

pub(crate) fn require_state(
    response: &Response,
    expected: ServiceState,
) -> Result<Generation, Box<dyn Error>> {
    require_ok(response, "status")?;
    let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
    if snapshot.state != expected {
        return Err(format!("expected state {expected:?}, received {:?}", snapshot.state).into());
    }
    response
        .generation
        .ok_or_else(|| "status generation is absent".into())
}

pub(crate) fn wait_for_state(
    socket: &Path,
    expected: ServiceState,
    generation: Option<Generation>,
    timeout: Duration,
) -> Result<Generation, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
        if snapshot.state == expected
            && generation.is_none_or(|generation| response.generation == Some(generation))
        {
            return Ok(response.generation.unwrap_or(Generation::FIRST));
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "service did not reach {expected:?}; last status was {snapshot:?}, generation {:?}",
                response.generation
            )
            .into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub(crate) fn wait_for_no_main_pid(socket: &Path, timeout: Duration) -> Result<(), Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        require_ok(&response, "descriptor status")?;
        if response
            .status
            .as_ref()
            .is_some_and(|snapshot| snapshot.main_pid.is_none())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err("descriptor launcher PID remained published".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub(crate) fn wait_for_state_and_logger(
    socket: &Path,
    expected_state: ServiceState,
    expected_logger: LoggerStatus,
    timeout: Duration,
) -> Result<Response, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
        if snapshot.state == expected_state && snapshot.logger == expected_logger {
            return Ok(response);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "service did not reach {expected_state:?}/{expected_logger:?}; last status was {snapshot:?}"
            )
            .into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub(crate) fn wait_for_new_ready(
    socket: &Path,
    previous: Generation,
    timeout: Duration,
) -> Result<Generation, Box<dyn Error>> {
    let deadline = Instant::now() + timeout;
    loop {
        let response = status(socket)?;
        let snapshot = response.status.as_ref().ok_or("status payload is absent")?;
        if snapshot.state == ServiceState::Ready
            && response
                .generation
                .is_some_and(|generation| generation != previous)
        {
            return response
                .generation
                .ok_or_else(|| "ready generation is absent".into());
        }
        if Instant::now() >= deadline {
            return Err("service did not publish a replacement ready generation".into());
        }
        thread::sleep(POLL_INTERVAL);
    }
}

pub(crate) fn wait_for_occurrences(
    path: &Path,
    pattern: &str,
    count: usize,
    timeout: Duration,
) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if fs::read_to_string(path).is_ok_and(|contents| contents.matches(pattern).count() >= count)
        {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        format!("marker did not contain {count} occurrences of {pattern:?}"),
    ))
}

pub(crate) fn wait_for_file(path: &Path, timeout: Duration) -> std::io::Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(POLL_INTERVAL);
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::TimedOut,
        "child PID file was not published",
    ))
}

pub(crate) fn assert_status(
    status: ExitStatus,
    expected: ExitClass,
    context: &str,
) -> Result<(), Box<dyn Error>> {
    if status.code() == Some(i32::from(expected.value())) {
        Ok(())
    } else {
        Err(format!(
            "{context} returned {status}; expected exit {}",
            expected.value()
        )
        .into())
    }
}
