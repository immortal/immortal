//! Dedicated single-threaded child-process broker and supervisor endpoint.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    os::fd::OwnedFd,
    os::unix::net::UnixStream as StdUnixStream,
    process,
    time::Duration,
};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{UnixStream, unix::OwnedWriteHalf},
    runtime::Builder,
    signal::unix::{Signal as ChildSignal, SignalKind, signal as listen_for_signal},
    sync::mpsc,
    task::JoinHandle,
    time::{Instant, timeout_at},
};

use crate::readiness::{ReadinessError, wait_for_ready as wait_for_readiness};
use crate::supervisor::Generation;

use super::{
    ChildEvent, ProcessCommand, ProcessGroupId, ProcessId, ProcessSignal, SignalTarget,
    SpawnFailure, SpawnStage, SpawnedProcess,
    broker_protocol::{
        BrokerEvent, BrokerProtocolError, BrokerReadinessFailure, BrokerRequest,
        BrokerSignalTarget, HEADER_BYTES, MAX_FRAME_BYTES, declared_frame_length,
    },
    reap_any_event, signal, spawn, spawn_with_descriptor,
};

const BROKER_EXIT_SOFTWARE: i32 = 70;
const SUPERVISOR_EVENT_CAPACITY: usize = 32;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_KILL_WAIT: Duration = Duration::from_secs(3);
const READINESS_DESCRIPTOR: i32 = 3;
const READINESS_EVENT_CAPACITY: usize = 32;
const AUXILIARY_GENERATION_BASE: u64 = 1_u64 << 63;

/// Supervisor-local identifier for a broker child which is not a service generation.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct BrokerTaskId(u64);

impl BrokerTaskId {
    /// Construct an auxiliary identifier from the nonzero low-half counter.
    #[must_use]
    pub const fn new(value: u64) -> Option<Self> {
        if value == 0 || value >= AUXILIARY_GENERATION_BASE {
            None
        } else {
            Some(Self(value))
        }
    }

    /// Return the supervisor-local task number.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    fn generation(self) -> Option<Generation> {
        Generation::new(AUXILIARY_GENERATION_BASE | self.0)
    }

    fn from_generation(generation: Generation) -> Option<Self> {
        let raw = generation.get();
        if raw & AUXILIARY_GENERATION_BASE == 0 {
            None
        } else {
            Self::new(raw & !AUXILIARY_GENERATION_BASE)
        }
    }
}

/// Main child or complete generation-group target resolved inside the broker.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BrokerSignalScope {
    Process,
    Group,
}

/// Stable reason a generation did not complete descriptor readiness.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReadinessFailure {
    Timeout,
    Descriptor,
    InvalidToken,
}

/// Typed observation delivered by the broker to its supervisor.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessBrokerEvent {
    Ready,
    Started {
        generation: Generation,
        process: ProcessId,
        group: ProcessGroupId,
    },
    SpawnFailed {
        generation: Generation,
        stage: SpawnStage,
        failure: SpawnFailure,
        os_error: Option<i32>,
        cleanup_pending: Option<ProcessId>,
    },
    Child {
        generation: Generation,
        event: ChildEvent,
    },
    TaskStarted {
        task: BrokerTaskId,
        process: ProcessId,
        group: ProcessGroupId,
    },
    TaskSpawnFailed {
        task: BrokerTaskId,
        stage: SpawnStage,
        failure: SpawnFailure,
        os_error: Option<i32>,
        cleanup_pending: Option<ProcessId>,
    },
    TaskChild {
        task: BrokerTaskId,
        event: ChildEvent,
    },
    SignalDelivered {
        generation: Generation,
    },
    SignalFailed {
        generation: Generation,
        os_error: Option<i32>,
    },
    TaskSignalDelivered {
        task: BrokerTaskId,
    },
    TaskSignalFailed {
        task: BrokerTaskId,
        os_error: Option<i32>,
    },
    Detached {
        generation: Generation,
    },
    DetachFailed {
        generation: Generation,
    },
    GenerationReady {
        generation: Generation,
    },
    ReadinessFailed {
        generation: Generation,
        failure: ReadinessFailure,
    },
    ShutdownComplete,
    ShutdownFailed {
        os_error: Option<i32>,
    },
}

impl From<BrokerEvent> for ProcessBrokerEvent {
    fn from(event: BrokerEvent) -> Self {
        match event {
            BrokerEvent::Ready => Self::Ready,
            BrokerEvent::Started {
                generation,
                process,
                group,
            } => BrokerTaskId::from_generation(generation).map_or(
                Self::Started {
                    generation,
                    process,
                    group,
                },
                |task| Self::TaskStarted {
                    task,
                    process,
                    group,
                },
            ),
            BrokerEvent::SpawnFailed {
                generation,
                stage,
                failure,
                os_error,
                cleanup_pending,
            } => BrokerTaskId::from_generation(generation).map_or(
                Self::SpawnFailed {
                    generation,
                    stage,
                    failure,
                    os_error,
                    cleanup_pending,
                },
                |task| Self::TaskSpawnFailed {
                    task,
                    stage,
                    failure,
                    os_error,
                    cleanup_pending,
                },
            ),
            BrokerEvent::Child { generation, event } => BrokerTaskId::from_generation(generation)
                .map_or(Self::Child { generation, event }, |task| Self::TaskChild {
                    task,
                    event,
                }),
            BrokerEvent::SignalDelivered { generation } => {
                BrokerTaskId::from_generation(generation)
                    .map_or(Self::SignalDelivered { generation }, |task| {
                        Self::TaskSignalDelivered { task }
                    })
            }
            BrokerEvent::SignalFailed {
                generation,
                os_error,
            } => BrokerTaskId::from_generation(generation).map_or(
                Self::SignalFailed {
                    generation,
                    os_error,
                },
                |task| Self::TaskSignalFailed { task, os_error },
            ),
            BrokerEvent::Detached { generation } => Self::Detached { generation },
            BrokerEvent::DetachFailed { generation } => Self::DetachFailed { generation },
            BrokerEvent::GenerationReady { generation } => Self::GenerationReady { generation },
            BrokerEvent::ReadinessFailed {
                generation,
                failure,
            } => Self::ReadinessFailed {
                generation,
                failure: match failure {
                    BrokerReadinessFailure::Timeout => ReadinessFailure::Timeout,
                    BrokerReadinessFailure::Descriptor => ReadinessFailure::Descriptor,
                    BrokerReadinessFailure::InvalidToken => ReadinessFailure::InvalidToken,
                },
            },
            BrokerEvent::ShutdownComplete => Self::ShutdownComplete,
            BrokerEvent::ShutdownFailed { os_error } => Self::ShutdownFailed { os_error },
        }
    }
}

/// Failure to create or communicate with the dedicated process broker.
#[derive(Debug)]
pub struct ProcessBrokerError(ProcessBrokerErrorKind);

#[derive(Debug)]
enum ProcessBrokerErrorKind {
    Io(io::Error),
    Protocol(BrokerProtocolError),
    SignalStreamClosed,
    UnownedChild(ProcessId),
}

impl ProcessBrokerError {
    fn is_end_of_stream(&self) -> bool {
        matches!(
            &self.0,
            ProcessBrokerErrorKind::Io(error)
                if error.kind() == io::ErrorKind::UnexpectedEof
                    || error.kind() == io::ErrorKind::ConnectionReset
        )
    }
}

impl Display for ProcessBrokerError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match &self.0 {
            ProcessBrokerErrorKind::Io(error) => write!(formatter, "broker I/O failed: {error}"),
            ProcessBrokerErrorKind::Protocol(error) => {
                write!(formatter, "broker protocol failed: {error}")
            }
            ProcessBrokerErrorKind::SignalStreamClosed => {
                formatter.write_str("broker SIGCHLD stream closed")
            }
            ProcessBrokerErrorKind::UnownedChild(process) => {
                write!(formatter, "broker reaped unowned child {process}")
            }
        }
    }
}

impl Error for ProcessBrokerError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match &self.0 {
            ProcessBrokerErrorKind::Io(error) => Some(error),
            ProcessBrokerErrorKind::Protocol(error) => Some(error),
            ProcessBrokerErrorKind::SignalStreamClosed
            | ProcessBrokerErrorKind::UnownedChild(_) => None,
        }
    }
}

impl From<io::Error> for ProcessBrokerError {
    fn from(error: io::Error) -> Self {
        Self(ProcessBrokerErrorKind::Io(error))
    }
}

impl From<BrokerProtocolError> for ProcessBrokerError {
    fn from(error: BrokerProtocolError) -> Self {
        Self(ProcessBrokerErrorKind::Protocol(error))
    }
}

/// Pre-runtime supervisor endpoint returned after forking the broker.
#[derive(Debug)]
pub struct ProcessBrokerEndpoint {
    process: ProcessId,
    socket: OwnedFd,
}

impl ProcessBrokerEndpoint {
    /// Return the direct broker child which the supervisor must eventually reap.
    #[must_use]
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// Register the endpoint with the already-created current-thread Tokio runtime.
    ///
    /// # Errors
    ///
    /// Returns an operating-system error while configuring or registering the socket.
    pub fn connect(self) -> Result<ProcessBrokerClient, ProcessBrokerError> {
        let stream = StdUnixStream::from(self.socket);
        stream.set_nonblocking(true)?;
        let stream = UnixStream::from_std(stream)?;
        let (mut reader, writer) = stream.into_split();
        let (event_sender, events) = mpsc::channel(SUPERVISOR_EVENT_CAPACITY);
        let reader_task = tokio::spawn(async move {
            loop {
                let event = read_event(&mut reader).await;
                let terminal = event.is_err();
                if event_sender.send(event).await.is_err() || terminal {
                    return;
                }
            }
        });
        Ok(ProcessBrokerClient {
            process: self.process,
            writer,
            events,
            reader_task,
        })
    }
}

/// Tokio-side client for one dedicated process broker.
#[derive(Debug)]
pub struct ProcessBrokerClient {
    process: ProcessId,
    writer: OwnedWriteHalf,
    events: mpsc::Receiver<Result<BrokerEvent, ProcessBrokerError>>,
    reader_task: JoinHandle<()>,
}

impl ProcessBrokerClient {
    /// Return the direct broker child which owns every service process.
    #[must_use]
    pub const fn process(&self) -> ProcessId {
        self.process
    }

    /// Request one fully materialized generation start.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Completion arrives through
    /// [`Self::next_event`].
    pub async fn spawn(
        &mut self,
        generation: Generation,
        command: ProcessCommand,
        startup_timeout: Duration,
    ) -> Result<(), ProcessBrokerError> {
        write_request(
            &mut self.writer,
            &BrokerRequest::Spawn {
                generation,
                command,
                startup_timeout,
                readiness_timeout: None,
            },
        )
        .await
    }

    /// Request a generation with one broker-owned readiness descriptor.
    ///
    /// The child receives descriptor 3 and `IMMORTAL_READY_FD=3`; readiness is
    /// reported only after the exact bounded token arrives before `timeout`.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Completion arrives through
    /// [`Self::next_event`].
    pub async fn spawn_with_readiness(
        &mut self,
        generation: Generation,
        command: ProcessCommand,
        startup_timeout: Duration,
        readiness_timeout: Duration,
    ) -> Result<(), ProcessBrokerError> {
        write_request(
            &mut self.writer,
            &BrokerRequest::Spawn {
                generation,
                command,
                startup_timeout,
                readiness_timeout: Some(readiness_timeout),
            },
        )
        .await
    }

    /// Request one auxiliary hook/logger task in its own process group.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Completion arrives through
    /// [`Self::next_event`].
    pub async fn spawn_task(
        &mut self,
        task: BrokerTaskId,
        command: ProcessCommand,
        startup_timeout: Duration,
    ) -> Result<(), ProcessBrokerError> {
        let generation = task.generation().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid auxiliary task ID")
        })?;
        write_request(
            &mut self.writer,
            &BrokerRequest::Spawn {
                generation,
                command,
                startup_timeout,
                readiness_timeout: None,
            },
        )
        .await
    }

    /// Signal an exact auxiliary task owned by the broker.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Delivery is acknowledged as
    /// a later task signal event.
    pub async fn signal_task(
        &mut self,
        task: BrokerTaskId,
        scope: BrokerSignalScope,
        signal: ProcessSignal,
    ) -> Result<(), ProcessBrokerError> {
        let generation = task.generation().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "invalid auxiliary task ID")
        })?;
        let target = match scope {
            BrokerSignalScope::Process => BrokerSignalTarget::Process,
            BrokerSignalScope::Group => BrokerSignalTarget::Group,
        };
        write_request(
            &mut self.writer,
            &BrokerRequest::Signal {
                generation,
                target,
                signal,
            },
        )
        .await
    }

    /// Request a signal against the exact generation currently owned by the broker.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Delivery is acknowledged as
    /// a later broker event.
    pub async fn signal(
        &mut self,
        generation: Generation,
        scope: BrokerSignalScope,
        signal: ProcessSignal,
    ) -> Result<(), ProcessBrokerError> {
        let target = match scope {
            BrokerSignalScope::Process => BrokerSignalTarget::Process,
            BrokerSignalScope::Group => BrokerSignalTarget::Group,
        };
        write_request(
            &mut self.writer,
            &BrokerRequest::Signal {
                generation,
                target,
                signal,
            },
        )
        .await
    }

    /// Relinquish one exact live generation without signaling or reaping it.
    ///
    /// This is used only for the explicit control operation that exits the
    /// supervisor while leaving its service running. Completion arrives as a
    /// later `Detached` or `DetachFailed` broker event.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error.
    pub async fn detach(&mut self, generation: Generation) -> Result<(), ProcessBrokerError> {
        write_request(&mut self.writer, &BrokerRequest::Detach { generation }).await
    }

    /// Request bounded termination and reaping of every broker-owned generation.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. The caller must wait for
    /// `ShutdownComplete` before reaping the broker.
    pub async fn shutdown(&mut self) -> Result<(), ProcessBrokerError> {
        write_request(&mut self.writer, &BrokerRequest::Shutdown).await
    }

    /// Receive the next independently framed broker observation.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Unexpected EOF means the
    /// broker died before completing its ownership obligations.
    pub async fn next_event(&mut self) -> Result<ProcessBrokerEvent, ProcessBrokerError> {
        match self.events.recv().await {
            Some(Ok(event)) => Ok(event.into()),
            Some(Err(error)) => Err(error),
            None => Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "process broker event stream closed",
            )
            .into()),
        }
    }
}

impl Drop for ProcessBrokerClient {
    fn drop(&mut self) {
        self.reader_task.abort();
    }
}

/// Fork a dedicated process broker before Tokio or any other thread is created.
///
/// The child never returns from this function. The parent receives a CLOEXEC
/// endpoint which it registers only after constructing its Tokio runtime.
///
/// # Errors
///
/// Returns a socket-pair or fork error in the supervisor process.
pub fn start_process_broker() -> io::Result<ProcessBrokerEndpoint> {
    let pair = fork::socket_pair_cloexec()?;
    let (supervisor_socket, broker_socket) = pair.into_parts();
    match fork::fork_process()? {
        fork::ProcessFork::Parent(process) => {
            drop(broker_socket);
            Ok(ProcessBrokerEndpoint {
                process: ProcessId(process.get()),
                socket: supervisor_socket,
            })
        }
        fork::ProcessFork::Child => {
            drop(supervisor_socket);
            let exit = match run_broker_process(broker_socket) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("immortal process broker: {error}");
                    BROKER_EXIT_SOFTWARE
                }
            };
            process::exit(exit);
        }
    }
}

fn run_broker_process(socket: OwnedFd) -> Result<(), ProcessBrokerError> {
    let stream = StdUnixStream::from(socket);
    stream.set_nonblocking(true)?;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let stream = UnixStream::from_std(stream)?;
        run_broker(stream).await
    })
}

async fn run_broker(stream: UnixStream) -> Result<(), ProcessBrokerError> {
    let (mut reader, mut writer) = stream.into_split();
    let mut child_signal = listen_for_signal(SignalKind::child())?;
    let mut generations = BTreeMap::new();
    let mut processes = BTreeMap::new();
    let (readiness_sender, mut readiness_events) = mpsc::channel(READINESS_EVENT_CAPACITY);
    write_event(&mut writer, &BrokerEvent::Ready).await?;

    loop {
        tokio::select! {
            request = read_request(&mut reader) => {
                let request = match request {
                    Ok(request) => request,
                    Err(error) if error.is_end_of_stream() => {
                        cleanup_after_supervisor_loss(
                            &mut child_signal,
                            &mut generations,
                            &mut processes,
                        ).await?;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                match request {
                    BrokerRequest::Spawn {
                        generation,
                        command,
                        startup_timeout,
                        readiness_timeout,
                    } => {
                        handle_spawn(
                            generation,
                            command,
                            startup_timeout,
                            readiness_timeout,
                            &mut writer,
                            BrokerSpawnState {
                                generations: &mut generations,
                                processes: &mut processes,
                                readiness_sender: &readiness_sender,
                            },
                        ).await?;
                    }
                    BrokerRequest::Signal { generation, target, signal: requested } => {
                        handle_signal(
                            generation,
                            target,
                            requested,
                            &mut writer,
                            &generations,
                        ).await?;
                    }
                    BrokerRequest::Detach { generation } => {
                        handle_detach(
                            generation,
                            &mut writer,
                            &mut generations,
                            &mut processes,
                        ).await?;
                    }
                    BrokerRequest::Shutdown => {
                        if shutdown_owned(
                            &mut child_signal,
                            &mut writer,
                            &mut generations,
                            &mut processes,
                        ).await? {
                            write_event(&mut writer, &BrokerEvent::ShutdownComplete).await?;
                            return Ok(());
                        }
                        write_event(
                            &mut writer,
                            &BrokerEvent::ShutdownFailed { os_error: None },
                        ).await?;
                    }
                }
            }
            signal = child_signal.recv() => {
                if signal.is_none() {
                    return Err(ProcessBrokerError(ProcessBrokerErrorKind::SignalStreamClosed));
                }
                forward_child_events(&mut writer, &mut generations, &mut processes).await?;
            }
            Some(observation) = readiness_events.recv() => {
                if generations.contains_key(&observation.generation) {
                    let event = match observation.result {
                        Ok(()) => BrokerEvent::GenerationReady {
                            generation: observation.generation,
                        },
                        Err(error) => BrokerEvent::ReadinessFailed {
                            generation: observation.generation,
                            failure: readiness_failure(&error),
                        },
                    };
                    write_event(&mut writer, &event).await?;
                }
            }
        }
    }
}

struct ReadinessObservation {
    generation: Generation,
    result: Result<(), ReadinessError>,
}

struct BrokerSpawnState<'a> {
    generations: &'a mut BTreeMap<Generation, SpawnedProcess>,
    processes: &'a mut BTreeMap<ProcessId, Generation>,
    readiness_sender: &'a mpsc::Sender<ReadinessObservation>,
}

fn readiness_failure(error: &ReadinessError) -> BrokerReadinessFailure {
    match error {
        ReadinessError::Timeout => BrokerReadinessFailure::Timeout,
        ReadinessError::Io(_) => BrokerReadinessFailure::Descriptor,
        ReadinessError::InvalidToken => BrokerReadinessFailure::InvalidToken,
    }
}

async fn handle_detach<W>(
    generation: Generation,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, SpawnedProcess>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let Some(child) = generations.get(&generation).copied() else {
        return write_event(writer, &BrokerEvent::DetachFailed { generation }).await;
    };
    if generations.len() != 1
        || processes.len() != 1
        || processes.get(&child.process()) != Some(&generation)
    {
        return write_event(writer, &BrokerEvent::DetachFailed { generation }).await;
    }
    generations.remove(&generation);
    processes.remove(&child.process());
    write_event(writer, &BrokerEvent::Detached { generation }).await
}

async fn handle_spawn<W>(
    generation: Generation,
    command: ProcessCommand,
    startup_timeout: Duration,
    readiness_timeout: Option<Duration>,
    writer: &mut W,
    state: BrokerSpawnState<'_>,
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
    let readiness = match prepare_readiness(command, readiness_timeout) {
        Ok(readiness) => readiness,
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
    let (spawn_result, readiness_waiter) = match readiness {
        PreparedReadiness::Immediate(command) => (spawn(command, startup_timeout), None),
        PreparedReadiness::Descriptor {
            command,
            child_descriptor,
            broker_stream,
            timeout,
        } => (
            spawn_with_descriptor(
                command,
                startup_timeout,
                Some((child_descriptor, READINESS_DESCRIPTOR)),
            ),
            Some((broker_stream, timeout)),
        ),
    };
    match spawn_result {
        Ok(child) => {
            if let Some((mut broker_stream, timeout)) = readiness_waiter {
                let sender = state.readiness_sender.clone();
                tokio::spawn(async move {
                    let result = wait_for_readiness(&mut broker_stream, timeout).await;
                    let _ = sender
                        .send(ReadinessObservation { generation, result })
                        .await;
                });
            }
            state.processes.insert(child.process(), generation);
            state.generations.insert(generation, child);
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
                state.processes.insert(process, generation);
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
        child_descriptor: OwnedFd,
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

async fn handle_signal<W>(
    generation: Generation,
    target: BrokerSignalTarget,
    requested: ProcessSignal,
    writer: &mut W,
    generations: &BTreeMap<Generation, SpawnedProcess>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let result = generations.get(&generation).map_or_else(
        || {
            Err(io::Error::new(
                io::ErrorKind::NotFound,
                "generation is not owned",
            ))
        },
        |child| {
            let target = match target {
                BrokerSignalTarget::Process => SignalTarget::Process(child.process()),
                BrokerSignalTarget::Group => SignalTarget::Group(child.group()),
            };
            signal(target, requested)
        },
    );
    let event = match result {
        Ok(()) => BrokerEvent::SignalDelivered { generation },
        Err(error) => BrokerEvent::SignalFailed {
            generation,
            os_error: error.raw_os_error(),
        },
    };
    write_event(writer, &event).await
}

async fn forward_child_events<W>(
    writer: &mut W,
    generations: &mut BTreeMap<Generation, SpawnedProcess>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    for event in collect_child_events(generations, processes)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

fn collect_child_events(
    generations: &mut BTreeMap<Generation, SpawnedProcess>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<Vec<BrokerEvent>, ProcessBrokerError> {
    let mut events = Vec::new();
    if processes.is_empty() {
        return Ok(events);
    }
    while !processes.is_empty() {
        let Some(event) = reap_any_event()? else {
            break;
        };
        let process = event.pid();
        let generation = processes.get(&process).copied().ok_or(ProcessBrokerError(
            ProcessBrokerErrorKind::UnownedChild(process),
        ))?;
        if event.is_terminal() {
            if let Some(child) = generations.get(&generation) {
                let _ = signal(SignalTarget::Group(child.group()), ProcessSignal::Kill);
            }
            processes.remove(&process);
            generations.remove(&generation);
        }
        events.push(BrokerEvent::Child { generation, event });
    }
    Ok(events)
}

async fn shutdown_owned<W>(
    child_signal: &mut ChildSignal,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, SpawnedProcess>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    signal_every_group(generations, ProcessSignal::Terminate);
    if reap_until(
        Instant::now() + SHUTDOWN_GRACE,
        child_signal,
        writer,
        generations,
        processes,
    )
    .await?
    {
        return Ok(true);
    }
    signal_every_group(generations, ProcessSignal::Kill);
    reap_until(
        Instant::now() + SHUTDOWN_KILL_WAIT,
        child_signal,
        writer,
        generations,
        processes,
    )
    .await
}

async fn reap_until<W>(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, SpawnedProcess>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    forward_child_events(writer, generations, processes).await?;
    while !processes.is_empty() {
        match timeout_at(deadline, child_signal.recv()).await {
            Ok(Some(())) => forward_child_events(writer, generations, processes).await?,
            Ok(None) => {
                return Err(ProcessBrokerError(
                    ProcessBrokerErrorKind::SignalStreamClosed,
                ));
            }
            Err(_) => return Ok(false),
        }
    }
    Ok(true)
}

fn signal_every_group(
    generations: &BTreeMap<Generation, SpawnedProcess>,
    requested: ProcessSignal,
) {
    for child in generations.values() {
        let _ = signal(SignalTarget::Group(child.group()), requested);
    }
}

async fn cleanup_after_supervisor_loss(
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, SpawnedProcess>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<(), ProcessBrokerError> {
    signal_every_group(generations, ProcessSignal::Kill);
    while !processes.is_empty() {
        if child_signal.recv().await.is_none() {
            return Err(ProcessBrokerError(
                ProcessBrokerErrorKind::SignalStreamClosed,
            ));
        }
        let _ = collect_child_events(generations, processes)?;
    }
    Ok(())
}

async fn write_request<W>(writer: &mut W, request: &BrokerRequest) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, &request.encode()?).await
}

async fn read_request<R>(reader: &mut R) -> Result<BrokerRequest, ProcessBrokerError>
where
    R: AsyncRead + Unpin,
{
    BrokerRequest::decode(&read_frame(reader).await?).map_err(Into::into)
}

async fn write_event<W>(writer: &mut W, event: &BrokerEvent) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, &event.encode()?).await
}

async fn read_event<R>(reader: &mut R) -> Result<BrokerEvent, ProcessBrokerError>
where
    R: AsyncRead + Unpin,
{
    BrokerEvent::decode(&read_frame(reader).await?).map_err(Into::into)
}

async fn write_frame<W>(writer: &mut W, frame: &[u8]) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(frame).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, ProcessBrokerError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0; HEADER_BYTES];
    let _ = reader.read_exact(&mut header).await?;
    let frame_length = declared_frame_length(&header)?;
    let payload_length = frame_length.saturating_sub(HEADER_BYTES);
    if frame_length > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame_length).into());
    }
    let mut frame = Vec::with_capacity(frame_length);
    frame.extend_from_slice(&header);
    frame.resize(frame_length, 0);
    let payload = frame
        .get_mut(HEADER_BYTES..)
        .ok_or(BrokerProtocolError::Truncated)?;
    if payload.len() != payload_length {
        return Err(BrokerProtocolError::LengthMismatch.into());
    }
    let _ = reader.read_exact(payload).await?;
    Ok(frame)
}
