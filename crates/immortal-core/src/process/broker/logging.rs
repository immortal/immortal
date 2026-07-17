//! Pre-runtime logging graph materialization and stable pipe ownership.
//!
//! The supervisor hands the broker a [`BrokerLoggingPlan`] before Tokio
//! starts. [`BrokerLogging::prepare`] opens one CLOEXEC pipe per local file
//! route and one for the optional shared logger, retaining the write end so
//! service descriptors can be cloned into every spawned child without ever
//! copying bytes itself. Closing the retained writers (`close_writer_masters`)
//! begins ordered EOF drain; child-held writer clones keep downstream input
//! alive until every upstream adapter has exited.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;

use crate::logging::OutputStream;
use crate::supervisor::Generation;

use super::{ProcessCommand, ProcessDescriptor};

const SHARED_LOGGER_PIPELINE: u16 = u16::MAX;

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

    pub(crate) const fn shared_logger() -> Self {
        Self::new(SHARED_LOGGER_PIPELINE, 0)
    }

    pub(crate) const fn is_shared_logger(self) -> bool {
        self.pipeline == SHARED_LOGGER_PIPELINE && self.stage == 0
    }
}

/// Fully materialized local file adapter for one service output stream.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct BrokerFileRoute {
    pub(crate) stream: OutputStream,
    pub(crate) command: ProcessCommand,
}

/// Logger graph transferred to the broker before Tokio starts.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct BrokerLoggingPlan {
    pub(crate) local_files: Vec<BrokerFileRoute>,
    pub(crate) logger: Option<ProcessCommand>,
}

pub(super) struct BrokerLogging {
    local_files: Vec<BrokerFileRouteRuntime>,
    logger: Option<SharedLoggerRuntime>,
    live_loggers: BTreeMap<BrokerLoggerId, Generation>,
    logger_generations: BTreeMap<Generation, BrokerLoggerId>,
}

struct BrokerFileRouteRuntime {
    stream: OutputStream,
    command: ProcessCommand,
    input: StablePipe,
}

struct SharedLoggerRuntime {
    command: ProcessCommand,
    input: StablePipe,
}

struct StablePipe {
    reader: OwnedFd,
    writer: Option<OwnedFd>,
}

impl BrokerLogging {
    pub(super) fn prepare(plan: BrokerLoggingPlan) -> io::Result<Self> {
        let mut local_files = Vec::with_capacity(plan.local_files.len());
        for route in plan.local_files {
            local_files.push(BrokerFileRouteRuntime {
                stream: route.stream,
                command: route.command,
                input: stable_pipe()?,
            });
        }
        let logger = if let Some(command) = plan.logger {
            Some(SharedLoggerRuntime {
                command,
                input: stable_pipe()?,
            })
        } else {
            None
        };
        Ok(Self {
            local_files,
            logger,
            live_loggers: BTreeMap::new(),
            logger_generations: BTreeMap::new(),
        })
    }

    pub(super) fn service_descriptors(&self) -> io::Result<Vec<ProcessDescriptor>> {
        if let Some(combined) = self
            .local_files
            .iter()
            .find(|route| route.stream == OutputStream::Combined)
        {
            let writer = clone_writer(&combined.input)?;
            return Ok(vec![
                ProcessDescriptor::map(writer.try_clone()?, libc::STDOUT_FILENO)?,
                ProcessDescriptor::map(writer, libc::STDERR_FILENO)?,
            ]);
        }

        let mut descriptors = Vec::with_capacity(2);
        self.push_service_descriptor(OutputStream::Stdout, libc::STDOUT_FILENO, &mut descriptors)?;
        self.push_service_descriptor(OutputStream::Stderr, libc::STDERR_FILENO, &mut descriptors)?;
        Ok(descriptors)
    }

    fn push_service_descriptor(
        &self,
        stream: OutputStream,
        target: i32,
        descriptors: &mut Vec<ProcessDescriptor>,
    ) -> io::Result<()> {
        let input = self
            .local_files
            .iter()
            .find(|route| route.stream == stream)
            .map(|route| &route.input)
            .or_else(|| self.logger.as_ref().map(|logger| &logger.input));
        if let Some(input) = input {
            descriptors.push(ProcessDescriptor::map(clone_writer(input)?, target)?);
        }
        Ok(())
    }

    pub(super) fn logger_command(
        &self,
        logger: BrokerLoggerId,
    ) -> io::Result<(ProcessCommand, Vec<ProcessDescriptor>)> {
        if logger.is_shared_logger() {
            let shared = self.logger.as_ref().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "shared logger is not configured",
                )
            })?;
            return Ok((
                shared.command.clone(),
                vec![ProcessDescriptor::map(
                    shared.input.reader.try_clone()?,
                    libc::STDIN_FILENO,
                )?],
            ));
        }
        if logger.stage != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unknown file-adapter stage",
            ));
        }
        let route = self
            .local_files
            .get(usize::from(logger.pipeline))
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "unknown file-adapter route")
            })?;
        let mut descriptors = vec![ProcessDescriptor::map(
            route.input.reader.try_clone()?,
            libc::STDIN_FILENO,
        )?];
        if let Some(shared) = &self.logger {
            descriptors.push(ProcessDescriptor::map(
                clone_writer(&shared.input)?,
                libc::STDOUT_FILENO,
            )?);
        }
        Ok((route.command.clone(), descriptors))
    }

    pub(super) fn register(
        &mut self,
        logger: BrokerLoggerId,
        generation: Generation,
    ) -> io::Result<()> {
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

    pub(super) fn is_available(&self, logger: BrokerLoggerId, generation: Generation) -> bool {
        !self.live_loggers.contains_key(&logger)
            && !self.logger_generations.contains_key(&generation)
    }

    pub(super) fn child_reaped(&mut self, generation: Generation) {
        if let Some(logger) = self.logger_generations.remove(&generation) {
            self.live_loggers.remove(&logger);
        }
    }

    pub(super) fn close_writer_masters(&mut self) {
        for route in &mut self.local_files {
            route.input.writer = None;
        }
        if let Some(logger) = &mut self.logger {
            logger.input.writer = None;
        }
    }
}

fn stable_pipe() -> io::Result<StablePipe> {
    let pipe = fork::pipe_cloexec()?;
    let (reader, writer) = pipe.into_parts();
    Ok(StablePipe {
        reader,
        writer: Some(writer),
    })
}

fn clone_writer(pipe: &StablePipe) -> io::Result<OwnedFd> {
    pipe.writer
        .as_ref()
        .ok_or_else(|| io::Error::new(io::ErrorKind::BrokenPipe, "logging input is closed"))?
        .try_clone()
}
