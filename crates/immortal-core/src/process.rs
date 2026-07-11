//! Child process creation, daemonization, ownership, environment, and signals.
//!
//! Unix fork, session, daemon, and wait primitives come from the `fork` crate.
//! This module owns the supervisor-facing wrappers so syscall details do not
//! leak into the CLI crates or the rest of the domain model.

use std::{collections::BTreeMap, ffi::OsString, io};

use crate::{
    config::{EnvironmentMode, ServiceConfig},
    supervisor::ChildResult,
};

/// Deterministic environment passed to a service or lifecycle hook.
pub type ProcessEnvironment = BTreeMap<OsString, OsString>;

/// Resolve the process environment without reading global state implicitly.
///
/// In inherited mode, entries are copied in iterator order and later duplicate
/// keys replace earlier ones. Configured UTF-8 values are then applied last. In
/// clear mode, only configured values are present. A caller can therefore take
/// one explicit snapshot of `std::env::vars_os()` before daemonization and use
/// the same inputs for every generation.
#[must_use]
pub fn resolve_environment(
    config: &ServiceConfig,
    inherited: impl IntoIterator<Item = (OsString, OsString)>,
) -> ProcessEnvironment {
    let mut resolved = if config.environment_mode == EnvironmentMode::Inherit {
        inherited.into_iter().collect()
    } else {
        ProcessEnvironment::new()
    };
    resolved.extend(
        config
            .environment
            .iter()
            .map(|(key, value)| (OsString::from(key), OsString::from(value))),
    );
    resolved
}

/// One terminated child drained from the canonical `fork` wait boundary.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ReapedChild {
    /// OS PID returned by `waitpid`; valid only for this completed wait event.
    pub pid: i32,
    /// Typed terminal result used by restart policy and status.
    pub result: ChildResult,
}

/// Drain one terminated child without blocking.
///
/// This intentionally observes only exit/signal termination because `fork`
/// 0.8 does not expose `WUNTRACED`/`WCONTINUED` waits. The future event loop
/// must call this repeatedly after a coalesced `SIGCHLD` until it returns
/// `Ok(None)` or the OS reports that no children remain.
///
/// # Errors
///
/// Returns an operating-system wait error or an invalid raw terminal status.
pub fn reap_any_terminated() -> io::Result<Option<ReapedChild>> {
    fork::wait_any_nohang()?
        .map(decode_wait_status_with_pid)
        .transpose()
}

fn decode_wait_status_with_pid((pid, status): (i32, i32)) -> io::Result<ReapedChild> {
    Ok(ReapedChild {
        pid,
        result: decode_wait_status(status)?,
    })
}

/// Decode the canonical fork crate's raw terminal status.
///
/// # Errors
///
/// Returns invalid data if the raw value is not exited/signalled or its code
/// cannot fit the public portable representation.
pub fn decode_wait_status(status: i32) -> io::Result<ChildResult> {
    if fork::WIFEXITED(status) {
        return u8::try_from(fork::WEXITSTATUS(status))
            .map(ChildResult::Exited)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "exit code is out of range"));
    }
    if fork::WIFSIGNALED(status) {
        return u8::try_from(fork::WTERMSIG(status))
            .map(ChildResult::Signaled)
            .map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "signal number is out of range")
            });
    }
    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "fork returned a non-terminal wait status",
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use crate::config::{EnvironmentMode, parse_str};

    use super::{
        ReapedChild, decode_wait_status, decode_wait_status_with_pid, resolve_environment,
    };
    use crate::supervisor::ChildResult;

    #[test]
    fn configured_values_override_one_explicit_inherited_snapshot()
    -> Result<(), Box<dyn std::error::Error>> {
        let config = parse_str(
            "version: 2\ncommand: [service]\nenvironment:\n  KEEP: configured\n  NEW: value\n",
        )?;
        let environment = resolve_environment(
            &config,
            [
                (OsString::from("KEEP"), OsString::from("old")),
                (OsString::from("BASE"), OsString::from("base")),
            ],
        );
        assert_eq!(
            environment.get(OsStr::new("KEEP")),
            Some(&OsString::from("configured"))
        );
        assert_eq!(
            environment.get(OsStr::new("BASE")),
            Some(&OsString::from("base"))
        );
        assert_eq!(
            environment.get(OsStr::new("NEW")),
            Some(&OsString::from("value"))
        );
        Ok(())
    }

    #[test]
    fn clear_mode_discards_every_inherited_entry() -> Result<(), Box<dyn std::error::Error>> {
        let mut config = parse_str("version: 2\ncommand: [service]\n")?;
        config.environment_mode = EnvironmentMode::Clear;
        config
            .environment
            .insert("ONLY".to_owned(), "configured".to_owned());
        let environment = resolve_environment(
            &config,
            [(OsString::from("SECRET"), OsString::from("inherited"))],
        );
        assert_eq!(environment.len(), 1);
        assert_eq!(
            environment.get(OsStr::new("ONLY")),
            Some(&OsString::from("configured"))
        );
        Ok(())
    }

    #[test]
    fn canonical_wait_statuses_become_typed_terminal_results()
    -> Result<(), Box<dyn std::error::Error>> {
        assert_eq!(decode_wait_status(0)?, ChildResult::Exited(0));
        assert_eq!(decode_wait_status(42_i32 << 8)?, ChildResult::Exited(42));
        assert_eq!(decode_wait_status(9)?, ChildResult::Signaled(9));
        assert_eq!(
            decode_wait_status_with_pid((123, 15))?,
            ReapedChild {
                pid: 123,
                result: ChildResult::Signaled(15),
            }
        );
        Ok(())
    }

    #[test]
    fn stopped_raw_status_is_not_misreported_as_termination() {
        // POSIX stopped status encoding: low byte 0x7f, stop signal in high byte.
        assert!(decode_wait_status((19_i32 << 8) | 0x7f).is_err());
    }
}
