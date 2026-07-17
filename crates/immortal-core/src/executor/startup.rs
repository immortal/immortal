//! Runtime ownership and daemon startup reporting helpers.
//!
//! Control ownership is acquired before broker creation and dropped only after
//! broker cleanup. Daemon startup reporting owns the one-shot readiness channel:
//! successful controlled startup consumes it exactly once, while initialization
//! failure converts the executor error chain back to an OS error for the parent
//! handshake without masking the original detached-child failure.

use std::{error::Error, io, path::Path};

use super::{DaemonStartup, ExecutorError, RuntimeOwner};

pub(super) struct ControlSetup {
    pub(super) owner: RuntimeOwner,
    pub(super) service_name: String,
}

impl ControlSetup {
    pub(super) fn acquire(directory: &Path) -> Result<Self, ExecutorError> {
        let owner = RuntimeOwner::acquire(directory)?;
        let service_name = owner
            .directory()
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid service name"))?
            .to_owned();
        Ok(Self {
            owner,
            service_name,
        })
    }
}

pub(super) enum StartupReporter {
    Foreground,
    Daemon(Option<DaemonStartup>),
}

impl StartupReporter {
    pub(super) fn notify_ready(&mut self) -> Result<(), ExecutorError> {
        match self {
            Self::Foreground => Ok(()),
            Self::Daemon(notifier) => notifier
                .take()
                .ok_or_else(|| io::Error::other("daemon readiness was already reported"))?
                .notify_ready()
                .map_err(Into::into),
        }
    }

    pub(super) fn fail_if_pending(&mut self, error: &ExecutorError) {
        let Self::Daemon(notifier) = self else {
            return;
        };
        if let Some(notifier) = notifier.take() {
            notifier.fail_and_exit(&startup_io_error(error));
        }
    }
}

pub(super) fn startup_io_error(error: &ExecutorError) -> io::Error {
    let mut current: &(dyn Error + 'static) = error;
    loop {
        if let Some(error) = current.downcast_ref::<io::Error>() {
            return error.raw_os_error().map_or_else(
                || io::Error::new(error.kind(), error.to_string()),
                io::Error::from_raw_os_error,
            );
        }
        let Some(source) = current.source() else {
            return io::Error::other(error.to_string());
        };
        current = source;
    }
}
