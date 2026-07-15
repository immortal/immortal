//! Bounded black-box contract for the manually runnable log probe.

use std::{
    error::Error,
    io::{self, Read},
    process::{Child, Command, ExitStatus, Stdio},
    str, thread,
    time::{Duration, Instant},
};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(2);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

struct CapturedOutput {
    status: ExitStatus,
    stderr: Vec<u8>,
    stdout: Vec<u8>,
}

struct ChildGuard {
    child: Option<Child>,
}

impl ChildGuard {
    const fn new(child: Child) -> Self {
        Self { child: Some(child) }
    }

    fn wait(mut self, timeout: Duration) -> io::Result<CapturedOutput> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::other("probe timeout exceeds the clock range"))?;
        loop {
            let status = self
                .child
                .as_mut()
                .ok_or_else(|| io::Error::other("probe child is absent"))?
                .try_wait()?;
            if let Some(status) = status {
                let mut child = self
                    .child
                    .take()
                    .ok_or_else(|| io::Error::other("probe child disappeared"))?;
                let stdout = read_pipe(child.stdout.take(), "stdout")?;
                let stderr = read_pipe(child.stderr.take(), "stderr")?;
                return Ok(CapturedOutput {
                    status,
                    stderr,
                    stdout,
                });
            }
            if Instant::now() >= deadline {
                self.terminate();
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "probe did not exit before its contract deadline",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }

    fn terminate(&mut self) {
        if let Some(child) = &mut self.child {
            let _ignored = child.kill();
            let _ignored = child.wait();
        }
        self.child = None;
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn read_pipe<R: Read>(pipe: Option<R>, name: &str) -> io::Result<Vec<u8>> {
    let mut pipe = pipe.ok_or_else(|| io::Error::other(format!("probe {name} pipe is absent")))?;
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes)?;
    Ok(bytes)
}

fn run_probe(arguments: &[&str]) -> io::Result<CapturedOutput> {
    let child = Command::new(env!("CARGO_BIN_EXE_immortal-log-probe"))
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    ChildGuard::new(child).wait(PROCESS_TIMEOUT)
}

#[test]
fn finite_probe_flushes_both_streams_and_returns_configured_status() -> Result<(), Box<dyn Error>> {
    let output = run_probe(&[
        "--interval",
        "1s",
        "--exit-after",
        "20ms",
        "--exit-code",
        "23",
    ])?;
    assert_eq!(output.status.code(), Some(23));
    assert_stream(str::from_utf8(&output.stdout)?, "stdout", 23)?;
    assert_stream(str::from_utf8(&output.stderr)?, "stderr", 23)?;
    Ok(())
}

#[test]
fn malformed_probe_arguments_fail_with_usage_diagnostic() -> Result<(), Box<dyn Error>> {
    let output = run_probe(&["--exit-code", "1"])?;
    assert_eq!(output.status.code(), Some(64));
    assert!(output.stdout.is_empty());
    let stderr = str::from_utf8(&output.stderr)?;
    assert!(stderr.contains("--exit-code requires --exit-after"));
    assert!(stderr.contains("immortal-log-probe --help"));
    Ok(())
}

fn assert_stream(content: &str, stream: &str, exit_code: u8) -> Result<(), Box<dyn Error>> {
    let mut lines = content.lines();
    let tick = lines
        .next()
        .ok_or_else(|| io::Error::other(format!("{stream} tick record is missing")))?;
    let exit = lines
        .next()
        .ok_or_else(|| io::Error::other(format!("{stream} exit record is missing")))?;
    if lines.next().is_some() {
        return Err(io::Error::other(format!("{stream} has excess records")).into());
    }
    let tick_prefix = format!("stream={stream} event=tick sequence=1 pid=");
    let exit_prefix = format!("stream={stream} event=exit sequence=2 pid=");
    if !tick.starts_with(&tick_prefix) || !tick.contains(" elapsed_ms=") {
        return Err(io::Error::other(format!("invalid {stream} tick record: {tick}")).into());
    }
    if !exit.starts_with(&exit_prefix)
        || !exit.contains(" elapsed_ms=")
        || !exit.ends_with(&format!(" exit_code={exit_code}"))
    {
        return Err(io::Error::other(format!("invalid {stream} exit record: {exit}")).into());
    }
    Ok(())
}
