//! One fresh process proving the pre-Tokio broker and its IPC lifecycle.

#[path = "support/broker_guard.rs"]
mod broker_guard;

use std::{
    error::Error,
    ffi::OsString,
    fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use immortal_core::{
    process::{
        BrokerSignalScope, BrokerTaskId, ChildEvent, ProcessBrokerEvent, ProcessCommand,
        ProcessCredentials, ProcessEnvironment, ProcessSignal, ReadinessFailure, SpawnFailure,
        SpawnStage, SupplementaryGroups, start_process_broker,
    },
    supervisor::Generation,
};
use tokio::runtime::Builder;

use crate::broker_guard::BrokerGuard;

const STARTUP_TIMEOUT: Duration = Duration::from_secs(2);
const EVENT_TIMEOUT: Duration = Duration::from_secs(3);
const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// Environment entries and per-entry bytes used to build one oversized `Spawn`
/// frame, large enough that its payload cannot arrive in a single socket write.
const LARGE_FRAME_VARIABLES: usize = 256;
const LARGE_FRAME_VARIABLE_BYTES: usize = 512;

#[allow(clippy::too_many_lines)]
fn main() -> Result<(), Box<dyn Error>> {
    let endpoint = start_process_broker()?;
    let mut broker = BrokerGuard::new(endpoint.process(), EVENT_TIMEOUT, POLL_INTERVAL);
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

        let stopped = generation(8)?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("5");
        client.spawn(stopped, command, STARTUP_TIMEOUT).await?;
        assert_started(client.next_event().await?, stopped)?;
        for (stop_signal, expected_signal) in [
            (ProcessSignal::Stop, u8::try_from(libc::SIGSTOP)?),
            (ProcessSignal::TerminalInput, u8::try_from(libc::SIGTTIN)?),
            (ProcessSignal::TerminalOutput, u8::try_from(libc::SIGTTOU)?),
        ] {
            client
                .signal(stopped, BrokerSignalScope::Group, stop_signal)
                .await?;
            assert_signal_delivered(client.next_event().await?, stopped)?;
            let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
            assert_child_stopped(event, stopped, expected_signal)?;

            client
                .signal(stopped, BrokerSignalScope::Group, ProcessSignal::Continue)
                .await?;
            assert_signal_delivered(client.next_event().await?, stopped)?;
            let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
            assert_child_continued(event, stopped)?;
        }
        client
            .signal(stopped, BrokerSignalScope::Group, ProcessSignal::Terminate)
            .await?;
        assert_signal_delivered(client.next_event().await?, stopped)?;
        let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_signaled(event, stopped)?;

        let tracked = generation(9)?;
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument("(/bin/sleep 0.15) & exit 0");
        client
            .spawn_with_lifetime(tracked, command, STARTUP_TIMEOUT, None)
            .await?;
        assert_started(client.next_event().await?, tracked)?;
        let exited = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_exit(exited, tracked, 0)?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::LifetimeClosed { generation } if generation == tracked => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected inherited lifetime closure, received {event:?}"
                ))
                .into());
            }
        }
        client
            .signal(tracked, BrokerSignalScope::Group, ProcessSignal::Terminate)
            .await?;
        match client.next_event().await? {
            ProcessBrokerEvent::SignalFailed { generation, .. } if generation == tracked => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected closed lifetime to reject raw signals, received {event:?}"
                ))
                .into());
            }
        }

        let invalid_lifetime = generation(10)?;
        let mut command = ProcessCommand::new("/bin/sh");
        command
            .argument("-c")
            .argument("eval \"printf x >&$IMMORTAL_LIFETIME_FD\"; exec /bin/sleep 5");
        client
            .spawn_with_lifetime(invalid_lifetime, command, STARTUP_TIMEOUT, None)
            .await?;
        assert_started(client.next_event().await?, invalid_lifetime)?;
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::LifetimeFailed { generation } if generation == invalid_lifetime => {
            }
            event => {
                return Err(io::Error::other(format!(
                    "expected lifetime protocol failure, received {event:?}"
                ))
                .into());
            }
        }
        client
            .signal(
                invalid_lifetime,
                BrokerSignalScope::Group,
                ProcessSignal::Kill,
            )
            .await?;
        assert_signal_delivered(client.next_event().await?, invalid_lifetime)?;
        let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
        assert_child_signaled(event, invalid_lifetime)?;

        let task = BrokerTaskId::new(1).ok_or("invalid auxiliary task ID")?;
        client
            .spawn_task(task, ProcessCommand::new("/usr/bin/true"), STARTUP_TIMEOUT)
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

        let killed_task = BrokerTaskId::new(2).ok_or("invalid auxiliary task ID")?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("5");
        client
            .spawn_task(killed_task, command, STARTUP_TIMEOUT)
            .await?;
        match client.next_event().await? {
            ProcessBrokerEvent::TaskStarted { task, .. } if task == killed_task => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected long auxiliary task start, received {event:?}"
                ))
                .into());
            }
        }
        tokio::time::sleep(Duration::from_millis(1_200)).await;
        client
            .signal_task(killed_task, BrokerSignalScope::Group, ProcessSignal::Kill)
            .await?;
        match client.next_event().await? {
            ProcessBrokerEvent::TaskSignalDelivered { task } if task == killed_task => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected auxiliary kill acknowledgement, received {event:?}"
                ))
                .into());
            }
        }
        match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
            ProcessBrokerEvent::TaskChild {
                task,
                event: ChildEvent::Signaled { .. },
            } if task == killed_task => {}
            event => {
                return Err(io::Error::other(format!(
                    "expected killed auxiliary task completion, received {event:?}"
                ))
                .into());
            }
        }

        // Regression: a large `Spawn` frame is written across several socket
        // writes, so the broker's request read spans many `select!` polls
        // while the reap tick and `SIGCHLD` from a concurrently exiting child
        // keep firing. Decoding inline lost the buffered header on every
        // cancellation and failed the frame's magic check.
        let churn = generation(11)?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("0.05");
        client.spawn(churn, command, STARTUP_TIMEOUT).await?;
        assert_started(client.next_event().await?, churn)?;

        let large = generation(12)?;
        let mut command = ProcessCommand::new("/bin/sleep");
        command.argument("0.05");
        let mut environment = ProcessEnvironment::new();
        for index in 0..LARGE_FRAME_VARIABLES {
            environment.insert(
                OsString::from(format!("IMMORTAL_LARGE_FRAME_{index}")),
                OsString::from("x".repeat(LARGE_FRAME_VARIABLE_BYTES)),
            );
        }
        command.environment(environment);
        client.spawn(large, command, STARTUP_TIMEOUT).await?;
        let mut churn_exited = false;
        let mut large_exited = false;
        let mut large_started = false;
        while !large_started {
            match tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await?? {
                ProcessBrokerEvent::Started { generation, .. } if generation == large => {
                    large_started = true;
                }
                ProcessBrokerEvent::Child {
                    generation,
                    event: ChildEvent::Exited { code: 0, .. },
                } if generation == churn => churn_exited = true,
                event => {
                    return Err(io::Error::other(format!(
                        "expected the large frame to spawn under child churn, received {event:?}"
                    ))
                    .into());
                }
            }
        }
        // Both workloads sleep for the same span, so their exit events race.
        // The property under test is that the large frame decoded intact, not
        // the order in which two independent children are reaped.
        while !churn_exited {
            let event = tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??;
            match event {
                ProcessBrokerEvent::Child {
                    generation,
                    event: ChildEvent::Exited { code: 0, .. },
                } if generation == churn => churn_exited = true,
                ProcessBrokerEvent::Child {
                    generation,
                    event: ChildEvent::Exited { code: 0, .. },
                } if generation == large => large_exited = true,
                event => {
                    return Err(io::Error::other(format!(
                        "expected the churn generation to exit, received {event:?}"
                    ))
                    .into());
                }
            }
        }
        if !large_exited {
            assert_child_exit(
                tokio::time::timeout(EVENT_TIMEOUT, client.next_event()).await??,
                large,
                0,
            )?;
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
    broker.wait()?;
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

fn assert_signal_delivered(
    event: ProcessBrokerEvent,
    expected: Generation,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::SignalDelivered { generation } if generation == expected => Ok(()),
        event => Err(io::Error::other(format!(
            "expected generation {expected:?} signal acknowledgement, received {event:?}"
        ))
        .into()),
    }
}

fn assert_child_stopped(
    event: ProcessBrokerEvent,
    expected: Generation,
    expected_signal: u8,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Stopped { signal, .. },
        } if generation == expected && signal == expected_signal => Ok(()),
        event => Err(io::Error::other(format!(
            "expected generation {expected:?} to stop from signal {expected_signal}, received {event:?}"
        ))
        .into()),
    }
}

fn assert_child_continued(
    event: ProcessBrokerEvent,
    expected: Generation,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Continued { .. },
        } if generation == expected => Ok(()),
        event => Err(io::Error::other(format!(
            "expected generation {expected:?} to continue, received {event:?}"
        ))
        .into()),
    }
}

fn assert_child_signaled(
    event: ProcessBrokerEvent,
    expected: Generation,
) -> Result<(), Box<dyn Error>> {
    match event {
        ProcessBrokerEvent::Child {
            generation,
            event: ChildEvent::Signaled { .. },
        } if generation == expected => Ok(()),
        event => Err(io::Error::other(format!(
            "expected generation {expected:?} to terminate from a signal, received {event:?}"
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
