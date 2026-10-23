//! Service and logger spawn preparation, group containment, and readiness or
//! lifetime descriptor wiring.
//!
//! [`handle_spawn`] and [`handle_logger_spawn`] are the two entry points
//! dispatch calls into. Each spawn first reserves a process group with an
//! out-of-group guard helper, whose unreaped anchor keeps the group and its ID
//! reserved while the guard is armed, joins the workload process into it, then
//! activates the guard: a helper event with the workload still owned is a
//! containment failure, not an ordinary exit, and the broker kills the
//! affected group. A spawn which fails once the group is reserved drops the
//! guard, which kills the group while the anchor still pins it, and retires
//! the group so shutdown awaits any member still exiting. Readiness and
//! lifetime tracking each materialize one dedicated socket pair mapped into
//! the child before it forks.
//!
//! Registering a generation also releases any retiring entry for its group
//! ID, before the `Started` acknowledgement is written: a new group can lead
//! an ID only after the old group drained, and a write failure must not leave
//! a stale entry that makes a kill path skip the new live group.

use std::collections::BTreeMap;
use std::io;
use std::net::Shutdown;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWrite};
use tokio::net::UnixStream;

use crate::readiness::wait_for_ready as wait_for_readiness;
use crate::supervisor::Generation;

use super::error::ProcessBrokerError;
use super::logging::{BrokerLoggerId, BrokerLogging};
use super::retiring::RetiringGroups;
use super::state::{
    BrokerGeneration, BrokerOwnedProcess, BrokerRuntimeState, BrokerSpawnState,
    LifetimeObservation, LifetimeState, ReadinessObservation,
};
use super::wire::write_event;
use super::{
    BrokerEvent, ProcessCommand, ProcessDescriptor, ProcessGroupGuard, ProcessGroupId, ProcessId,
    SpawnError, SpawnFailure, SpawnStage, SpawnedProcess, spawn_with_descriptors_in_group,
};

pub(super) const GROUP_GUARD_CLEANUP_TIMEOUT: Duration = Duration::from_secs(3);
const READINESS_DESCRIPTOR: i32 = 3;
const LIFETIME_DESCRIPTOR: i32 = 4;

pub(super) async fn handle_spawn<W>(
    generation: Generation,
    command: ProcessCommand,
    startup_timeout: Duration,
    readiness_timeout: Option<Duration>,
    lifetime_tracking: bool,
    writer: &mut W,
    mut state: BrokerSpawnState<'_>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    if state.generations.contains_key(&generation) {
        return write_event(
            writer,
            &BrokerEvent::SpawnFailed {
                generation,
                stage: SpawnStage::Specification,
                failure: SpawnFailure::InvalidForkContract,
                os_error: None,
                cleanup_pending: None,
            },
        )
        .await;
    }
    let prepared =
        match prepare_service_spawn(command, readiness_timeout, lifetime_tracking, state.logging) {
            Ok(prepared) => prepared,
            Err(error) => {
                return write_event(
                    writer,
                    &BrokerEvent::SpawnFailed {
                        generation,
                        stage: SpawnStage::DescriptorMapping,
                        failure: SpawnFailure::OperatingSystem,
                        os_error: error.raw_os_error(),
                        cleanup_pending: None,
                    },
                )
                .await;
            }
        };
    let PreparedServiceSpawn {
        command,
        descriptors,
        lifetime_waiter,
        readiness_waiter,
    } = prepared;
    let spawn_result = spawn_guarded(command, startup_timeout, descriptors, state.retiring);
    match spawn_result {
        Ok(guarded) => {
            let event = register_spawned_service(
                generation,
                guarded,
                lifetime_waiter,
                lifetime_tracking,
                readiness_waiter,
                &mut state,
            );
            write_event(writer, &event).await
        }
        Err(error) => {
            if let Some(process) = error.cleanup_pending() {
                state
                    .processes
                    .insert(process, BrokerOwnedProcess::Generation(generation));
            }
            write_event(
                writer,
                &BrokerEvent::SpawnFailed {
                    generation,
                    stage: error.stage(),
                    failure: error.failure(),
                    os_error: error.raw_os_error(),
                    cleanup_pending: error.cleanup_pending(),
                },
            )
            .await
        }
    }
}

fn register_spawned_service(
    generation: Generation,
    guarded: GuardedSpawn,
    lifetime_waiter: Option<UnixStream>,
    lifetime_tracking: bool,
    readiness_waiter: Option<(UnixStream, Duration)>,
    state: &mut BrokerSpawnState<'_>,
) -> BrokerEvent {
    let child = guarded.child;
    if let Some((mut broker_stream, timeout)) = readiness_waiter {
        let sender = state.readiness_sender.clone();
        tokio::spawn(async move {
            let result = wait_for_readiness(&mut broker_stream, timeout).await;
            let _ = sender
                .send(ReadinessObservation { generation, result })
                .await;
        });
    }
    if let Some(mut broker_stream) = lifetime_waiter {
        let sender = state.lifetime_sender.clone();
        tokio::spawn(async move {
            let result = wait_for_lifetime_close(&mut broker_stream).await;
            let _ = sender
                .send(LifetimeObservation { generation, result })
                .await;
        });
    }
    state
        .processes
        .insert(child.process(), BrokerOwnedProcess::Generation(generation));
    state
        .processes
        .insert(guarded.guard_process, BrokerOwnedProcess::Guard(generation));
    register_generation(
        state.generations,
        state.retiring,
        generation,
        BrokerGeneration {
            child: Some(child),
            group: child.group(),
            guard: Some(guarded.guard),
            lifetime: if lifetime_tracking {
                LifetimeState::Tracking
            } else {
                LifetimeState::Foreground
            },
        },
    );
    BrokerEvent::Started {
        generation,
        process: child.process(),
        group: child.group(),
    }
}

/// Record a newly spawned generation as owned.
///
/// A new group can lead its ID only after every member of an older group with
/// that ID was reaped, so any retiring entry for the ID is already satisfied.
/// It is forgotten here, before any fallible acknowledgement I/O, so no kill
/// path can mistake the new live group for the drained one.
fn register_generation(
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    retiring: &mut RetiringGroups,
    generation: Generation,
    entry: BrokerGeneration,
) {
    retiring.forget(entry.group);
    generations.insert(generation, entry);
}

struct PreparedServiceSpawn {
    command: ProcessCommand,
    descriptors: Vec<ProcessDescriptor>,
    lifetime_waiter: Option<UnixStream>,
    readiness_waiter: Option<(UnixStream, Duration)>,
}

struct GuardedSpawn {
    child: SpawnedProcess,
    guard: ProcessGroupGuard,
    guard_process: ProcessId,
}

/// Spawn `command` into a freshly reserved group under an armed guard.
///
/// A failure once the group is reserved drops the guard, whose helper kills
/// the group while its anchor still pins the ID, so the group is retired for
/// shutdown to await any member which is still exiting. A failure to reserve
/// leaves no group to retire.
///
/// # Errors
///
/// Returns the reservation, spawn, or activation failure, naming the child
/// left for the broker to reap when one exists.
fn spawn_guarded(
    command: ProcessCommand,
    startup_timeout: Duration,
    descriptors: impl IntoIterator<Item = ProcessDescriptor>,
    retiring: &mut RetiringGroups,
) -> Result<GuardedSpawn, SpawnError> {
    let guard = ProcessGroupGuard::new().map_err(|error| guard_spawn_error(error, None))?;
    let group = guard
        .group()
        .map_err(|error| guard_spawn_error(error, None))?;
    let joined = join_guarded_group(command, startup_timeout, descriptors, guard, group);
    if joined.is_err() {
        retiring.retire(group);
    }
    joined
}

fn join_guarded_group(
    command: ProcessCommand,
    startup_timeout: Duration,
    descriptors: impl IntoIterator<Item = ProcessDescriptor>,
    mut guard: ProcessGroupGuard,
    group: ProcessGroupId,
) -> Result<GuardedSpawn, SpawnError> {
    let child = spawn_with_descriptors_in_group(command, startup_timeout, descriptors, group)?;
    guard
        .activate(GROUP_GUARD_CLEANUP_TIMEOUT)
        .map_err(|error| guard_spawn_error(error, Some(child.process())))?;
    let guard_process = guard
        .process()
        .map_err(|error| guard_spawn_error(error, Some(child.process())))?;
    Ok(GuardedSpawn {
        child,
        guard,
        guard_process,
    })
}

fn guard_spawn_error(error: io::Error, cleanup_pending: Option<ProcessId>) -> SpawnError {
    SpawnError {
        stage: SpawnStage::ProcessGroup,
        failure: SpawnFailure::OperatingSystem,
        cleanup_pending,
        source: Some(error),
    }
}

fn prepare_service_spawn(
    command: ProcessCommand,
    readiness_timeout: Option<Duration>,
    lifetime_tracking: bool,
    logging: &BrokerLogging,
) -> io::Result<PreparedServiceSpawn> {
    let mut descriptors = logging.service_descriptors()?;
    let (command, readiness_waiter) = match prepare_readiness(command, readiness_timeout)? {
        PreparedReadiness::Immediate(command) => (command, None),
        PreparedReadiness::Descriptor {
            command,
            child_descriptor,
            broker_stream,
            timeout,
        } => {
            descriptors.push(child_descriptor);
            (command, Some((broker_stream, timeout)))
        }
    };
    let (command, lifetime_waiter) =
        prepare_lifetime(command, lifetime_tracking, &mut descriptors)?;
    Ok(PreparedServiceSpawn {
        command,
        descriptors,
        lifetime_waiter,
        readiness_waiter,
    })
}

pub(super) async fn handle_logger_spawn<W>(
    generation: Generation,
    logger: BrokerLoggerId,
    startup_timeout: Duration,
    writer: &mut W,
    state: &mut BrokerRuntimeState,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let BrokerRuntimeState {
        generations,
        logging,
        processes,
        retiring,
        ..
    } = state;
    if generations.contains_key(&generation) || !logging.is_available(logger, generation) {
        return write_event(
            writer,
            &BrokerEvent::SpawnFailed {
                generation,
                stage: SpawnStage::Specification,
                failure: SpawnFailure::InvalidForkContract,
                os_error: None,
                cleanup_pending: None,
            },
        )
        .await;
    }
    let (command, descriptors) = match logging.logger_command(logger) {
        Ok(prepared) => prepared,
        Err(error) => {
            return write_event(
                writer,
                &BrokerEvent::SpawnFailed {
                    generation,
                    stage: SpawnStage::DescriptorMapping,
                    failure: SpawnFailure::OperatingSystem,
                    os_error: error.raw_os_error(),
                    cleanup_pending: None,
                },
            )
            .await;
        }
    };
    match spawn_guarded(command, startup_timeout, descriptors, retiring) {
        Ok(guarded) => {
            let child = guarded.child;
            logging.register(logger, generation)?;
            processes.insert(child.process(), BrokerOwnedProcess::Generation(generation));
            processes.insert(guarded.guard_process, BrokerOwnedProcess::Guard(generation));
            register_generation(
                generations,
                retiring,
                generation,
                BrokerGeneration {
                    child: Some(child),
                    group: child.group(),
                    guard: Some(guarded.guard),
                    lifetime: LifetimeState::Foreground,
                },
            );
            write_event(
                writer,
                &BrokerEvent::Started {
                    generation,
                    process: child.process(),
                    group: child.group(),
                },
            )
            .await
        }
        Err(error) => {
            if let Some(process) = error.cleanup_pending() {
                processes.insert(process, BrokerOwnedProcess::Generation(generation));
            }
            write_event(
                writer,
                &BrokerEvent::SpawnFailed {
                    generation,
                    stage: error.stage(),
                    failure: error.failure(),
                    os_error: error.raw_os_error(),
                    cleanup_pending: error.cleanup_pending(),
                },
            )
            .await
        }
    }
}

enum PreparedReadiness {
    Immediate(ProcessCommand),
    Descriptor {
        command: ProcessCommand,
        child_descriptor: ProcessDescriptor,
        broker_stream: UnixStream,
        timeout: Duration,
    },
}

fn prepare_readiness(
    mut command: ProcessCommand,
    timeout: Option<Duration>,
) -> io::Result<PreparedReadiness> {
    let Some(timeout) = timeout else {
        return Ok(PreparedReadiness::Immediate(command));
    };
    command.environment_variable("IMMORTAL_READY_FD", READINESS_DESCRIPTOR.to_string());
    let pair = fork::socket_pair_cloexec()?;
    let (broker_descriptor, child_descriptor) = pair.into_parts();
    let child_descriptor = ProcessDescriptor::map(child_descriptor, READINESS_DESCRIPTOR)?;
    let stream = StdUnixStream::from(broker_descriptor);
    stream.set_nonblocking(true)?;
    let broker_stream = UnixStream::from_std(stream)?;
    Ok(PreparedReadiness::Descriptor {
        command,
        child_descriptor,
        broker_stream,
        timeout,
    })
}

fn prepare_lifetime(
    mut command: ProcessCommand,
    enabled: bool,
    descriptors: &mut Vec<ProcessDescriptor>,
) -> io::Result<(ProcessCommand, Option<UnixStream>)> {
    if !enabled {
        return Ok((command, None));
    }
    command.environment_variable("IMMORTAL_LIFETIME_FD", LIFETIME_DESCRIPTOR.to_string());
    let pair = fork::socket_pair_cloexec()?;
    let (broker_descriptor, child_descriptor) = pair.into_parts();
    descriptors.push(ProcessDescriptor::map(
        child_descriptor,
        LIFETIME_DESCRIPTOR,
    )?);
    let stream = StdUnixStream::from(broker_descriptor);
    stream.shutdown(Shutdown::Write)?;
    stream.set_nonblocking(true)?;
    Ok((command, Some(UnixStream::from_std(stream)?)))
}

async fn wait_for_lifetime_close(stream: &mut UnixStream) -> io::Result<()> {
    let mut unexpected = [0_u8; 1];
    match stream.read(&mut unexpected).await? {
        0 => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "lifetime descriptor received data before closing",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error;

    use crate::supervisor::Generation;

    use super::super::ProcessGroupId;
    use super::{BrokerGeneration, LifetimeState, RetiringGroups, register_generation};

    /// Registration itself releases a stale retiring entry for the new group's
    /// ID, so no later failure can leave a kill path skipping the live group.
    #[test]
    fn registering_a_generation_releases_a_stale_retiring_entry() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(1).ok_or("invalid test generation")?;
        let reused = ProcessGroupId::try_from(4_242)?;
        let unrelated = ProcessGroupId::try_from(4_343)?;
        let mut generations = BTreeMap::new();
        let mut retiring = RetiringGroups::default();
        retiring.retire(reused);
        retiring.retire(unrelated);

        register_generation(
            &mut generations,
            &mut retiring,
            generation,
            BrokerGeneration {
                child: None,
                guard: None,
                group: reused,
                lifetime: LifetimeState::Foreground,
            },
        );
        assert!(generations.contains_key(&generation));
        assert!(!retiring.contains(reused));
        assert!(
            retiring.contains(unrelated),
            "only the reused ID is released"
        );
        Ok(())
    }
}
