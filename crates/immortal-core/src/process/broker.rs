//! Dedicated single-threaded child-process broker and supervisor endpoint.
//!
//! The broker exclusively owns direct children, process groups, descriptor
//! endpoints, waits, and supervisor-loss cleanup. Descriptor generations retain
//! logical ownership after their launcher exits; EOF and the pre-runtime stop
//! plan are handled without adopting an application PID. `SIGCHLD` drives
//! immediate reaping, while a low-frequency sweep closes platform notification
//! gaps through the same ownership-checked wait path.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    net::Shutdown,
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
    time::{Instant, MissedTickBehavior, interval_at, timeout_at},
};

use crate::logging::OutputStream;
use crate::readiness::{ReadinessError, wait_for_ready as wait_for_readiness};
use crate::supervisor::Generation;

use super::{
    ChildEvent, ProcessCommand, ProcessDescriptor, ProcessGroupId, ProcessId, ProcessSignal,
    SignalTarget, SpawnFailure, SpawnStage, SpawnedProcess,
    broker_protocol::{
        BrokerEvent, BrokerProtocolError, BrokerReadinessFailure, BrokerRequest,
        BrokerSignalTarget, HEADER_BYTES, MAX_FRAME_BYTES, declared_frame_length,
    },
    reap_any_event, signal, spawn, spawn_with_descriptors,
};

const BROKER_EXIT_SOFTWARE: i32 = 70;
const CHILD_REAP_INTERVAL: Duration = Duration::from_millis(250);
const SUPERVISOR_EVENT_CAPACITY: usize = 32;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);
const SHUTDOWN_KILL_WAIT: Duration = Duration::from_secs(3);
const READINESS_DESCRIPTOR: i32 = 3;
const READINESS_EVENT_CAPACITY: usize = 32;
const LIFETIME_DESCRIPTOR: i32 = 4;
const LIFETIME_EVENT_CAPACITY: usize = 32;
const MAX_LIFETIME_CLEANUP_TIMEOUT: Duration = Duration::from_hours(24);
const AUXILIARY_GENERATION_BASE: u64 = 1_u64 << 63;

/// Bounded address of one logger stage inside the broker-owned graph.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct BrokerLoggerId {
    pipeline: u16,
    stage: u16,
}

impl BrokerLoggerId {
    pub(crate) const fn new(pipeline: u16, stage: u16) -> Self {
        Self { pipeline, stage }
    }

    pub(crate) const fn pipeline(self) -> u16 {
        self.pipeline
    }

    pub(crate) const fn stage(self) -> u16 {
        self.stage
    }
}

/// Fully materialized logger commands for one service output stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BrokerLoggerPipeline {
    pub(crate) stream: OutputStream,
    pub(crate) stages: Vec<ProcessCommand>,
}

/// Logger graph transferred to the broker before Tokio starts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BrokerLoggingPlan {
    pub(crate) pipelines: Vec<BrokerLoggerPipeline>,
}

struct BrokerLogging {
    pipelines: Vec<BrokerPipeline>,
    live_loggers: BTreeMap<BrokerLoggerId, Generation>,
    logger_generations: BTreeMap<Generation, BrokerLoggerId>,
}

struct BrokerPipeline {
    stream: OutputStream,
    stages: Vec<ProcessCommand>,
    pipes: Vec<StablePipe>,
}

struct StablePipe {
    reader: OwnedFd,
    writer: Option<OwnedFd>,
}

impl BrokerLogging {
    fn prepare(plan: BrokerLoggingPlan) -> io::Result<Self> {
        let mut pipelines = Vec::with_capacity(plan.pipelines.len());
        for pipeline in plan.pipelines {
            if pipeline.stages.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "broker logging pipeline has no stages",
                ));
            }
            let mut pipes = Vec::with_capacity(pipeline.stages.len());
            for _ in 0..pipeline.stages.len() {
                let pipe = fork::pipe_cloexec()?;
                let (reader, writer) = pipe.into_parts();
                pipes.push(StablePipe {
                    reader,
                    writer: Some(writer),
                });
            }
            pipelines.push(BrokerPipeline {
                stream: pipeline.stream,
                stages: pipeline.stages,
                pipes,
            });
        }
        Ok(Self {
            pipelines,
            live_loggers: BTreeMap::new(),
            logger_generations: BTreeMap::new(),
        })
    }

    fn service_descriptors(&self) -> io::Result<Vec<ProcessDescriptor>> {
        let mut descriptors = Vec::with_capacity(self.pipelines.len().saturating_mul(2));
        for pipeline in &self.pipelines {
            let writer = pipeline
                .pipes
                .first()
                .ok_or_else(|| io::Error::other("broker logging pipeline has no input pipe"))?
                .writer
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "logging input is closed")
                })?
                .try_clone()?;
            match pipeline.stream {
                OutputStream::Stdout => {
                    descriptors.push(ProcessDescriptor::map(writer, libc::STDOUT_FILENO)?);
                }
                OutputStream::Stderr => {
                    descriptors.push(ProcessDescriptor::map(writer, libc::STDERR_FILENO)?);
                }
                OutputStream::Combined => {
                    descriptors.push(ProcessDescriptor::map(
                        writer.try_clone()?,
                        libc::STDOUT_FILENO,
                    )?);
                    descriptors.push(ProcessDescriptor::map(writer, libc::STDERR_FILENO)?);
                }
            }
        }
        Ok(descriptors)
    }

    fn logger_command(
        &self,
        logger: BrokerLoggerId,
    ) -> io::Result<(ProcessCommand, Vec<ProcessDescriptor>)> {
        let pipeline = self
            .pipelines
            .get(usize::from(logger.pipeline))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unknown logger pipeline")
            })?;
        let stage = usize::from(logger.stage);
        let command = pipeline
            .stages
            .get(stage)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unknown logger stage"))?
            .clone();
        let input = pipeline
            .pipes
            .get(stage)
            .ok_or_else(|| io::Error::other("logger input pipe is absent"))?
            .reader
            .try_clone()?;
        let mut descriptors = vec![ProcessDescriptor::map(input, libc::STDIN_FILENO)?];
        if let Some(next) = stage
            .checked_add(1)
            .and_then(|index| pipeline.pipes.get(index))
        {
            descriptors.push(ProcessDescriptor::map(
                next.writer
                    .as_ref()
                    .ok_or_else(|| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "logger output is closed")
                    })?
                    .try_clone()?,
                libc::STDOUT_FILENO,
            )?);
        }
        Ok((command, descriptors))
    }

    fn register(&mut self, logger: BrokerLoggerId, generation: Generation) -> io::Result<()> {
        if self.live_loggers.contains_key(&logger)
            || self.logger_generations.contains_key(&generation)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "logger slot or generation is already active",
            ));
        }
        self.live_loggers.insert(logger, generation);
        self.logger_generations.insert(generation, logger);
        Ok(())
    }

    fn is_available(&self, logger: BrokerLoggerId, generation: Generation) -> bool {
        !self.live_loggers.contains_key(&logger)
            && !self.logger_generations.contains_key(&generation)
    }

    fn child_reaped(&mut self, generation: Generation) {
        if let Some(logger) = self.logger_generations.remove(&generation) {
            self.live_loggers.remove(&logger);
        }
    }

    fn close_writer_masters(&mut self) {
        for pipeline in &mut self.pipelines {
            for pipe in &mut pipeline.pipes {
                pipe.writer = None;
            }
        }
    }
}

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

/// Broker-owned fallback needed to stop a descriptor generation after supervisor loss.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BrokerLifetimePlan {
    stop: ProcessCommand,
    stop_timeout: Duration,
    lifetime_timeout: Duration,
}

impl BrokerLifetimePlan {
    /// Build the materialized stop command and its two hard deadlines.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` when either deadline is zero or exceeds 24 hours.
    pub fn new(
        stop: ProcessCommand,
        stop_timeout: Duration,
        lifetime_timeout: Duration,
    ) -> io::Result<Self> {
        if stop_timeout.is_zero()
            || lifetime_timeout.is_zero()
            || stop_timeout > MAX_LIFETIME_CLEANUP_TIMEOUT
            || lifetime_timeout > MAX_LIFETIME_CLEANUP_TIMEOUT
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "broker lifetime cleanup deadlines must be greater than zero and at most 24 hours",
            ));
        }
        Ok(Self {
            stop,
            stop_timeout,
            lifetime_timeout,
        })
    }
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
    LifetimeClosed {
        generation: Generation,
    },
    LifetimeFailed {
        generation: Generation,
    },
    LoggerInputsClosed,
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
            BrokerEvent::LifetimeClosed { generation } => Self::LifetimeClosed { generation },
            BrokerEvent::LifetimeFailed { generation } => Self::LifetimeFailed { generation },
            BrokerEvent::LoggerInputsClosed => Self::LoggerInputsClosed,
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
                lifetime_tracking: false,
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
                lifetime_tracking: false,
            },
        )
        .await
    }

    /// Request one generation whose logical lifetime is represented by descriptor 4.
    ///
    /// The child receives `IMMORTAL_LIFETIME_FD=4`. The broker retains the
    /// peer endpoint and reports closure only after every inherited child
    /// endpoint has closed.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Completion arrives through
    /// [`Self::next_event`].
    pub async fn spawn_with_lifetime(
        &mut self,
        generation: Generation,
        command: ProcessCommand,
        startup_timeout: Duration,
        readiness_timeout: Option<Duration>,
    ) -> Result<(), ProcessBrokerError> {
        write_request(
            &mut self.writer,
            &BrokerRequest::Spawn {
                generation,
                command,
                startup_timeout,
                readiness_timeout,
                lifetime_tracking: true,
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
                lifetime_tracking: false,
            },
        )
        .await
    }

    /// Request one configured logger stage in its own process group.
    ///
    /// The broker resolves `logger` against its pre-runtime graph and maps
    /// clones of the stable pipe endpoints into the child. Completion arrives
    /// through the ordinary task events for `task`.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error.
    pub(crate) async fn spawn_logger(
        &mut self,
        task: BrokerTaskId,
        logger: BrokerLoggerId,
        startup_timeout: Duration,
    ) -> Result<(), ProcessBrokerError> {
        let generation = task
            .generation()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid logger task ID"))?;
        write_request(
            &mut self.writer,
            &BrokerRequest::SpawnLogger {
                generation,
                logger,
                startup_timeout,
            },
        )
        .await
    }

    /// Close the broker's retained logging writer endpoints.
    ///
    /// Existing child descriptor clones remain valid; once their upstream
    /// process exits, downstream loggers observe EOF and may drain normally.
    ///
    /// # Errors
    ///
    /// Returns a bounded protocol or socket error. Completion is acknowledged
    /// by [`ProcessBrokerEvent::LoggerInputsClosed`].
    pub(crate) async fn close_logger_inputs(&mut self) -> Result<(), ProcessBrokerError> {
        write_request(&mut self.writer, &BrokerRequest::CloseLoggerInputs).await
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
    start_process_broker_with_logging(BrokerLoggingPlan::default(), None)
}

/// Fork a process broker with one pre-runtime descriptor-cleanup contract.
///
/// The broker invokes this stop command only if its supervisor connection is
/// lost while a descriptor-tracked generation remains active.
///
/// # Errors
///
/// Returns a socket-pair or fork error in the supervisor process.
pub fn start_process_broker_with_lifetime(
    lifetime: BrokerLifetimePlan,
) -> io::Result<ProcessBrokerEndpoint> {
    start_process_broker_with_logging(BrokerLoggingPlan::default(), Some(lifetime))
}

pub(crate) fn start_process_broker_with_logging(
    logging: BrokerLoggingPlan,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
) -> io::Result<ProcessBrokerEndpoint> {
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
            let exit = match run_broker_process(broker_socket, logging, lifetime_cleanup) {
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

fn run_broker_process(
    socket: OwnedFd,
    logging: BrokerLoggingPlan,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
) -> Result<(), ProcessBrokerError> {
    let logging = BrokerLogging::prepare(logging)?;
    let stream = StdUnixStream::from(socket);
    stream.set_nonblocking(true)?;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let stream = UnixStream::from_std(stream)?;
        run_broker(stream, logging, lifetime_cleanup).await
    })
}

async fn run_broker(
    stream: UnixStream,
    logging: BrokerLogging,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
) -> Result<(), ProcessBrokerError> {
    let (mut reader, mut writer) = stream.into_split();
    let mut child_signal = listen_for_signal(SignalKind::child())?;
    let (readiness_sender, mut readiness_events) = mpsc::channel(READINESS_EVENT_CAPACITY);
    let (lifetime_sender, mut lifetime_events) = mpsc::channel(LIFETIME_EVENT_CAPACITY);
    let mut child_reap = interval_at(Instant::now() + CHILD_REAP_INTERVAL, CHILD_REAP_INTERVAL);
    child_reap.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut state = BrokerRuntimeState {
        generations: BTreeMap::new(),
        logging,
        lifetime_cleanup,
        lifetime_sender,
        processes: BTreeMap::new(),
        readiness_sender,
    };
    write_event(&mut writer, &BrokerEvent::Ready).await?;

    loop {
        tokio::select! {
            request = read_request(&mut reader) => {
                let request = match request {
                    Ok(request) => request,
                    Err(error) if error.is_end_of_stream() => {
                        cleanup_after_supervisor_loss(
                            &mut child_signal,
                            &mut state.generations,
                            &mut state.processes,
                            &mut state.logging,
                            &mut lifetime_events,
                            state.lifetime_cleanup.take(),
                        ).await?;
                        return Ok(());
                    }
                    Err(error) => return Err(error),
                };
                if handle_broker_request(request, &mut child_signal, &mut writer, &mut state).await? {
                    return Ok(());
                }
            }
            signal = child_signal.recv() => {
                if signal.is_none() {
                    return Err(ProcessBrokerError(ProcessBrokerErrorKind::SignalStreamClosed));
                }
                forward_child_events(
                    &mut writer,
                    &mut state.generations,
                    &mut state.processes,
                    &mut state.logging,
                ).await?;
            }
            _ = child_reap.tick(), if !state.processes.is_empty() => {
                forward_child_events(
                    &mut writer,
                    &mut state.generations,
                    &mut state.processes,
                    &mut state.logging,
                ).await?;
            }
            Some(observation) = readiness_events.recv() => {
                if state.generations.contains_key(&observation.generation) {
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
            Some(observation) = lifetime_events.recv() => {
                handle_lifetime_observation(observation, &mut writer, &mut state.generations).await?;
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LifetimeState {
    Foreground,
    Tracking,
    Closed,
    Failed,
}

struct BrokerGeneration {
    child: Option<SpawnedProcess>,
    lifetime: LifetimeState,
}

struct BrokerRuntimeState {
    generations: BTreeMap<Generation, BrokerGeneration>,
    logging: BrokerLogging,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
    lifetime_sender: mpsc::Sender<LifetimeObservation>,
    processes: BTreeMap<ProcessId, Generation>,
    readiness_sender: mpsc::Sender<ReadinessObservation>,
}

async fn handle_broker_request<W>(
    request: BrokerRequest,
    child_signal: &mut ChildSignal,
    writer: &mut W,
    state: &mut BrokerRuntimeState,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    match request {
        BrokerRequest::Spawn {
            generation,
            command,
            startup_timeout,
            readiness_timeout,
            lifetime_tracking,
        } => {
            handle_spawn(
                generation,
                command,
                startup_timeout,
                readiness_timeout,
                lifetime_tracking,
                writer,
                BrokerSpawnState {
                    generations: &mut state.generations,
                    lifetime_sender: &state.lifetime_sender,
                    processes: &mut state.processes,
                    readiness_sender: &state.readiness_sender,
                    logging: &state.logging,
                },
            )
            .await?;
        }
        BrokerRequest::SpawnLogger {
            generation,
            logger,
            startup_timeout,
        } => {
            handle_logger_spawn(
                generation,
                logger,
                startup_timeout,
                writer,
                &mut state.generations,
                &mut state.processes,
                &mut state.logging,
            )
            .await?;
        }
        BrokerRequest::CloseLoggerInputs => {
            state.logging.close_writer_masters();
            write_event(writer, &BrokerEvent::LoggerInputsClosed).await?;
        }
        BrokerRequest::Signal {
            generation,
            target,
            signal: requested,
        } => {
            handle_signal(generation, target, requested, writer, &state.generations).await?;
        }
        BrokerRequest::Detach { generation } => {
            handle_detach(
                generation,
                writer,
                &mut state.generations,
                &mut state.processes,
            )
            .await?;
        }
        BrokerRequest::Shutdown => {
            let complete = shutdown_owned(
                child_signal,
                writer,
                &mut state.generations,
                &mut state.processes,
                &mut state.logging,
            )
            .await?;
            let event = if complete {
                BrokerEvent::ShutdownComplete
            } else {
                BrokerEvent::ShutdownFailed { os_error: None }
            };
            write_event(writer, &event).await?;
            return Ok(complete);
        }
    }
    Ok(false)
}

struct ReadinessObservation {
    generation: Generation,
    result: Result<(), ReadinessError>,
}

struct LifetimeObservation {
    generation: Generation,
    result: io::Result<()>,
}

struct BrokerSpawnState<'a> {
    generations: &'a mut BTreeMap<Generation, BrokerGeneration>,
    lifetime_sender: &'a mpsc::Sender<LifetimeObservation>,
    processes: &'a mut BTreeMap<ProcessId, Generation>,
    readiness_sender: &'a mpsc::Sender<ReadinessObservation>,
    logging: &'a BrokerLogging,
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
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    let Some(child) = generations
        .get(&generation)
        .filter(|entry| entry.lifetime == LifetimeState::Foreground)
        .and_then(|entry| entry.child)
    else {
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
    lifetime_tracking: bool,
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
    let spawn_result = if prepared.descriptors.is_empty() {
        spawn(prepared.command, startup_timeout)
    } else {
        spawn_with_descriptors(prepared.command, startup_timeout, prepared.descriptors)
    };
    match spawn_result {
        Ok(child) => {
            if let Some((mut broker_stream, timeout)) = prepared.readiness_waiter {
                let sender = state.readiness_sender.clone();
                tokio::spawn(async move {
                    let result = wait_for_readiness(&mut broker_stream, timeout).await;
                    let _ = sender
                        .send(ReadinessObservation { generation, result })
                        .await;
                });
            }
            if let Some(mut broker_stream) = prepared.lifetime_waiter {
                let sender = state.lifetime_sender.clone();
                tokio::spawn(async move {
                    let result = wait_for_lifetime_close(&mut broker_stream).await;
                    let _ = sender
                        .send(LifetimeObservation { generation, result })
                        .await;
                });
            }
            state.processes.insert(child.process(), generation);
            state.generations.insert(
                generation,
                BrokerGeneration {
                    child: Some(child),
                    lifetime: if lifetime_tracking {
                        LifetimeState::Tracking
                    } else {
                        LifetimeState::Foreground
                    },
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

struct PreparedServiceSpawn {
    command: ProcessCommand,
    descriptors: Vec<ProcessDescriptor>,
    lifetime_waiter: Option<UnixStream>,
    readiness_waiter: Option<(UnixStream, Duration)>,
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

async fn handle_logger_spawn<W>(
    generation: Generation,
    logger: BrokerLoggerId,
    startup_timeout: Duration,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
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
    match spawn_with_descriptors(command, startup_timeout, descriptors) {
        Ok(child) => {
            logging.register(logger, generation)?;
            processes.insert(child.process(), generation);
            generations.insert(
                generation,
                BrokerGeneration {
                    child: Some(child),
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
                processes.insert(process, generation);
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

async fn handle_lifetime_observation<W>(
    observation: LifetimeObservation,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(event) = record_lifetime_observation(&observation, generations) {
        write_event(writer, &event).await?;
    }
    Ok(())
}

fn record_lifetime_observation(
    observation: &LifetimeObservation,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
) -> Option<BrokerEvent> {
    let entry = generations.get_mut(&observation.generation)?;
    if entry.lifetime != LifetimeState::Tracking {
        return None;
    }
    let event = if observation.result.is_ok() {
        entry.lifetime = LifetimeState::Closed;
        BrokerEvent::LifetimeClosed {
            generation: observation.generation,
        }
    } else {
        entry.lifetime = LifetimeState::Failed;
        BrokerEvent::LifetimeFailed {
            generation: observation.generation,
        }
    };
    let generation_finished = entry.child.is_none();
    if generation_finished {
        generations.remove(&observation.generation);
    }
    Some(event)
}

async fn handle_signal<W>(
    generation: Generation,
    target: BrokerSignalTarget,
    requested: ProcessSignal,
    writer: &mut W,
    generations: &BTreeMap<Generation, BrokerGeneration>,
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
        |entry| {
            let child = entry.child.ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "generation has no direct child")
            })?;
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
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    for event in collect_child_events(generations, processes, logging)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

fn collect_child_events(
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
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
            processes.remove(&process);
            let remove_generation = if let Some(entry) = generations.get_mut(&generation) {
                if entry.lifetime == LifetimeState::Foreground {
                    if let Some(child) = entry.child {
                        let _ = signal(SignalTarget::Group(child.group()), ProcessSignal::Kill);
                    }
                    true
                } else {
                    entry.child = None;
                    matches!(
                        entry.lifetime,
                        LifetimeState::Closed | LifetimeState::Failed
                    )
                }
            } else {
                false
            };
            if remove_generation {
                generations.remove(&generation);
                logging.child_reaped(generation);
            }
        }
        events.push(BrokerEvent::Child { generation, event });
    }
    Ok(events)
}

async fn shutdown_owned<W>(
    child_signal: &mut ChildSignal,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
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
        logging,
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
        logging,
    )
    .await
}

async fn reap_until<W>(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
) -> Result<bool, ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    forward_child_events(writer, generations, processes, logging).await?;
    while !processes.is_empty() {
        match timeout_at(deadline, child_signal.recv()).await {
            Ok(Some(())) => {
                forward_child_events(writer, generations, processes, logging).await?;
            }
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
    generations: &BTreeMap<Generation, BrokerGeneration>,
    requested: ProcessSignal,
) {
    for child in generations.values().filter_map(|entry| entry.child) {
        let _ = signal(SignalTarget::Group(child.group()), requested);
    }
}

async fn cleanup_after_supervisor_loss(
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
) -> Result<(), ProcessBrokerError> {
    signal_every_group(generations, ProcessSignal::Kill);
    if !reap_without_events(
        Instant::now() + SHUTDOWN_KILL_WAIT,
        child_signal,
        generations,
        processes,
        logging,
    )
    .await?
    {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "broker children survived supervisor-loss cleanup",
        )
        .into());
    }

    let mut tracked = generations.iter().filter_map(|(generation, entry)| {
        (entry.lifetime != LifetimeState::Foreground).then_some(*generation)
    });
    let generation = tracked.next();
    if tracked.next().is_some() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "broker owns more than one descriptor-tracked generation",
        )
        .into());
    }
    let complete = match (generation, lifetime_cleanup) {
        (Some(generation), Some(cleanup)) => {
            run_supervisor_loss_hook(
                generation,
                cleanup,
                child_signal,
                generations,
                processes,
                logging,
                lifetime_events,
            )
            .await?
        }
        (None, _) => true,
        (Some(_), None) => false,
    };
    if complete {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "descriptor cleanup did not establish lifetime closure",
        )
        .into())
    }
}

async fn run_supervisor_loss_hook(
    generation: Generation,
    cleanup: BrokerLifetimePlan,
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
) -> Result<bool, ProcessBrokerError> {
    let BrokerLifetimePlan {
        stop,
        stop_timeout,
        lifetime_timeout,
    } = cleanup;
    let hook = match spawn(stop, SHUTDOWN_GRACE) {
        Ok(hook) => {
            processes.insert(hook.process(), generation);
            Some(hook)
        }
        Err(error) => {
            if let Some(process) = error.cleanup_pending() {
                processes.insert(process, generation);
            }
            None
        }
    };
    let hook_finished = reap_without_events(
        Instant::now() + stop_timeout,
        child_signal,
        generations,
        processes,
        logging,
    )
    .await?;
    if let Some(hook) = hook
        && !hook_finished
    {
        let _ = signal(SignalTarget::Group(hook.group()), ProcessSignal::Kill);
        let reaped = reap_without_events(
            Instant::now() + SHUTDOWN_KILL_WAIT,
            child_signal,
            generations,
            processes,
            logging,
        )
        .await?;
        if !reaped {
            return Ok(false);
        }
    }
    let lifetime_closed = wait_for_cleanup_lifetime(
        generation,
        Instant::now() + lifetime_timeout,
        generations,
        lifetime_events,
    )
    .await?;
    Ok(hook_finished && lifetime_closed)
}

async fn reap_without_events(
    deadline: Instant,
    child_signal: &mut ChildSignal,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, Generation>,
    logging: &mut BrokerLogging,
) -> Result<bool, ProcessBrokerError> {
    let _ = collect_child_events(generations, processes, logging)?;
    while !processes.is_empty() {
        match timeout_at(deadline, child_signal.recv()).await {
            Ok(Some(())) => {
                let _ = collect_child_events(generations, processes, logging)?;
            }
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

async fn wait_for_cleanup_lifetime(
    generation: Generation,
    deadline: Instant,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    lifetime_events: &mut mpsc::Receiver<LifetimeObservation>,
) -> Result<bool, ProcessBrokerError> {
    if !generations.contains_key(&generation) {
        return Ok(true);
    }
    loop {
        let observation = match timeout_at(deadline, lifetime_events.recv()).await {
            Ok(Some(observation)) => observation,
            Ok(None) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "broker lifetime event stream closed",
                )
                .into());
            }
            Err(_) => return Ok(false),
        };
        let observed_generation = observation.generation;
        let closed = observation.result.is_ok();
        let _ = record_lifetime_observation(&observation, generations);
        if observed_generation == generation {
            return Ok(closed && !generations.contains_key(&generation));
        }
    }
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
