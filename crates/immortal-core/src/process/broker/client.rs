//! Pre-runtime endpoint and Tokio-side client for one dedicated process broker.
//!
//! [`ProcessBrokerEndpoint`] is returned by forking the broker before Tokio
//! exists; [`ProcessBrokerEndpoint::connect`] registers the CLOEXEC socket
//! with the already-constructed current-thread runtime and spawns the
//! reader task that decodes framed [`BrokerEvent`]s into a bounded channel.
//! [`ProcessBrokerClient`] is the only supervisor-facing way to request a
//! spawn, signal, detach, or shutdown; its `Drop` aborts the reader task so
//! no orphaned task outlives the client.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::time::Duration;

use tokio::net::{UnixStream, unix::OwnedWriteHalf};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::supervisor::Generation;

use super::error::ProcessBrokerError;
use super::event::ProcessBrokerEvent;
use super::logging::BrokerLoggerId;
use super::types::{BrokerSignalScope, BrokerTaskId};
use super::wire::{read_event, write_request};
use super::{
    BrokerEvent, BrokerRequest, BrokerSignalTarget, ProcessCommand, ProcessId, ProcessSignal,
};

const SUPERVISOR_EVENT_CAPACITY: usize = 32;

/// Pre-runtime supervisor endpoint returned after forking the broker.
#[derive(Debug)]
pub struct ProcessBrokerEndpoint {
    pub(super) process: ProcessId,
    pub(super) socket: OwnedFd,
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
    /// Build a client over an already-connected socket for in-crate tests.
    ///
    /// Lets executor dispatch be exercised against a synthetic peer without
    /// forking a real broker. Test-only, so no production path can create a
    /// client whose peer is not the broker it reaps.
    #[cfg(test)]
    pub(crate) fn for_test(stream: UnixStream, process: ProcessId) -> Self {
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
        Self {
            process,
            writer,
            events,
            reader_task,
        }
    }

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
