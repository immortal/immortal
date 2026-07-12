//! One fresh process proving the pre-Tokio broker and its IPC lifecycle.

use std::{
    error::Error,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

use immortal_core::{
    process::{
        BrokerSignalScope, BrokerTaskId, ChildEvent, ProcessBrokerEvent, ProcessCommand,
        ProcessCredentials, ProcessEnvironment, ProcessId, ProcessSignal, ReadinessFailure,
        SignalTarget, SpawnFailure, SpawnStage, SupplementaryGroups, reap_any_event, signal,
        start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process());
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async {
        let mut client = endpoint.connect()?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::Ready => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected broker readiness, received {event:?}"
                ))
                .into());
            }
        }

        let first = generation(1)?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("0.05");
        client.spawn(first, command, STARTUP_TIMEOUT).await?;
        assert_started(client.next_event().await?, first)?;
        let exited = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_exit(exited, first, 0)?;

        let failed = generation(2)?;
        client
            .spawn(
                failed,
                ProcessCommand::new("/definitely/not/an/immortal-broker-executable"),
                STARTUP_TIMEOUT,
            )
            .await?;
        assert_broker_exec_failure(client.next_event().await?, failed)?;

        let signaled = generation(3)?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("5");
        client.spawn(signaled, command, STARTUP_TIMEOUT).await?;
        assert_started(client.next_event().await?, signaled)?;
        client
            .signal(signaled, BrokerSignalScope::Group, ProcessSignal::Terminate)
            .await?;
        match client.next_event().await? {
            ProcessBrokerEvent::SignalDelivered { generation } if generation == signaled => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected signal acknowledgement, received {event:?}"
                ))
                .into());
            }
        }
        let terminated = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        match terminated {
            ProcessBrokerEvent::Child {
                generation,
                event: ChildEvent::Signaled { signal, .. },
            } if generation == signaled && signal > 0 => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected broker signal termination, received {event:?}"
                ))
                .into());
            }
        }

        let ready = generation(4)?;
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument("eval \"printf 'READY\\n' >&$IMMORTAL_READY_FD\"; sleep 0.05");
        client
            .spawn_with_readiness(ready, command, STARTUP_TIMEOUT, EVENT_TIMEOUT)
            .await?;
        assert_started(client.next_event().await?, ready)?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::GenerationReady { generation } if generation == ready => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected descriptor readiness, received {event:?}"
                ))
                .into());
            }
        }
        let exited = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_exit(exited, ready, 0)?;

        let invalid_ready = generation(5)?;
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument("eval \"printf 'WRONG\\n' >&$IMMORTAL_READY_FD\"; exec sleep 5");
        client
            .spawn_with_readiness(invalid_ready, command, STARTUP_TIMEOUT, EVENT_TIMEOUT)
            .await?;
        assert_started(client.next_event().await?, invalid_ready)?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::ReadinessFailed {
                generation,
                failure: ReadinessFailure::InvalidToken,
            } if generation == invalid_ready => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected invalid readiness token, received {event:?}"
                ))
                .into());
            }
        }
        client
            .signal(invalid_ready, BrokerSignalScope::Group, ProcessSignal::Kill)
            .await?;
        let _ = client.next_event().await?;
        let terminated = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        match terminated {
            ProcessBrokerEvent::Child {
                generation,
                event: ChildEvent::Signaled { .. },
            } if generation == invalid_ready => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected invalid-readiness cleanup, received {event:?}"
                ))
                .into());
            }
        }

        let descendant = generation(6)?;
        let marker = DescendantMarker::new();
        let mut environment = ProcessEnvironment::new();
        environment.insert(
            OsString::from("MARKER"),
            marker.path().as_os_str().to_os_string(),
        );
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument("(/bin/sleep 1; printf leaked > \"$MARKER\") & exit 0")
            .environment(environment);
        client.spawn(descendant, command, STARTUP_TIMEOUT).await?;
        assert_started(client.next_event().await?, descendant)?;
        let exited = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_exit(exited, descendant, 0)?;
        tokio::time::sleep(Duration::from_millis(1_250)).await;
        if marker.path().exists() {
            return Err(io::Error::other(
                "background descendant survived main-child group cleanup",
            )
            .into());
        }

        let credentialed = generation(7)?;
        let mut command = ProcessCommand::new("/usr/bin/id");
        command.argument("-u").credentials(ProcessCredentials::new(
            nix::unistd::geteuid().as_raw(),
            nix::unistd::getegid().as_raw(),
            SupplementaryGroups::Preserve,
        ));
        client.spawn(credentialed, command, STARTUP_TIMEOUT).await?;
        assert_started(client.next_event().await?, credentialed)?;
        let exited = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_exit(exited, credentialed, 0)?;

        let task = BrokerTaskId::new(1).ok_or("invalid auxiliary task ID")?;
        client
            .spawn_task(task, ProcessCommand::new("/bin/true"), STARTUP_TIMEOUT)
            .await?;
        match client.next_event().await? {
            ProcessBrokerEvent::TaskStarted { task: started, .. } if started == task => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected auxiliary task start, received {event:?}"
                ))
                .into());
            }
        }
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::TaskChild {
                task: completed,
                event: ChildEvent::Exited { code: 0, .. },
            } if completed == task => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected auxiliary task completion, received {event:?}"
                ))
                .into());
            }
        }

        client.shutdown().await?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::ShutdownComplete => Ok::<(), Box<dyn Error>>(()),
            event => Err(io::Error::other(format!(
                "expected broker shutdown completion, received {event:?}"
            ))
            .into()),
        }
    })?;
    drop(runtime);
    broker.wait(EVENT_TIMEOUT)?;
    Ok(())
}

fn generation(value: u64) -> Result<Generation, Box<dyn Error>> {
    Generation::new(value)
        .ok_or_else(|| io::Error::other("test generation must be nonzero"))
        .map_err(Into::into)
}

fn assert_started(event: ProcessBrokerEvent, expected: Generation) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Started { generation, .. } if generation == expected => Ok(()),
        event => Err(io::Error::other(format!(
            "expected generation {expected:?} to start, received {event:?}"
        ))
        .into()),
    }
}

fn assert_child_exit(
    event: ProcessBrokerEvent,
    expected: Generation,
    expected_code: u8,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Exited { code, .. },
        } if generation == expected && code == expected_code => Ok(()),
        event => Err(io::Error::other(format!(
            "expected generation exit code {expected_code}, received {event:?}"
        ))
        .into()),
    }
}

fn assert_broker_exec_failure(
    event: ProcessBrokerEvent,
    expected: Generation,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::SpawnFailed {
            generation,
            stage: SpawnStage::Execute,
            failure: SpawnFailure::OperatingSystem,
            cleanup_pending: None,
            ..
        } if generation == expected => Ok(()),
        event => Err(
            io::Error::other(format!("expected broker exec failure, received {event:?}")).into(),
        ),
    }
}

struct BrokerGuard {
    process: ProcessId,
    reaped: bool,
}

impl BrokerGuard {
    const fn new(process: ProcessId) -> Self {
        Self {
            process,
            reaped: false,
        }
    }

    fn wait(&mut self, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return match event {
                        ChildEvent::Exited { code: 0, .. } => Ok(()),
                        _ => Err(io::Error::other(format!(
                            "broker terminated unsuccessfully: {event:?}"
                        ))),
                    };
                }
                Ok(Some(event)) => {
                    return Err(io::Error::other(format!(
                        "supervisor reaped unexpected child event {event:?}"
                    )));
                }
                Ok(None) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for process broker",
                ));
            }
            thread::sleep(POLL_INTERVAL);
        }
    }
}

impl Drop for BrokerGuard {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        let _ = signal(SignalTarget::Process(self.process), ProcessSignal::Kill);
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while Instant::now() < deadline {
            match reap_any_event() {
                Ok(Some(event)) if event.pid() == self.process && event.is_terminal() => {
                    self.reaped = true;
                    return;
                }
                Ok(Some(_) | None) => thread::sleep(POLL_INTERVAL),
                Err(_) => return,
            }
        }
    }
}

struct DescendantMarker(PathBuf);

impl DescendantMarker {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("immortal-broker-descendant-{}", std::process::id()));
        let _ = fs::remove_file(&path);
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for DescendantMarker {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}
