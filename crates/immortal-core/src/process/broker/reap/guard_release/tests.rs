use std::cell::Cell;
use std::collections::BTreeMap;
use std::error::Error;
use std::io;

use crate::supervisor::Generation;

use super::super::super::logging::{BrokerLogging, BrokerLoggingPlan};
use super::super::super::retiring::RetiringGroups;
use super::super::super::state::BrokerOwnedProcess;
use super::super::correlate_owned_event;
use super::{
    ChildEvent, GuardRelease, ProcessId, collect_released_helper_with, release_guard_with,
};

const HELPER_PID: i32 = 4242;

fn helper() -> Result<ProcessId, Box<dyn Error>> {
    ProcessId::new(HELPER_PID).ok_or_else(|| "invalid test helper".into())
}

fn generation() -> Result<Generation, Box<dyn Error>> {
    Generation::new(7).ok_or_else(|| "invalid test generation".into())
}

#[test]
fn a_completed_release_is_released_without_a_probe() -> Result<(), Box<dyn Error>> {
    let mut processes = BTreeMap::new();
    let probed = Cell::new(false);
    let release = release_guard_with(
        generation()?,
        helper()?,
        &mut processes,
        || Ok(()),
        |_| {
            probed.set(true);
            Ok(None)
        },
    )?;
    assert_eq!(release, GuardRelease::Released);
    assert!(!probed.get());
    assert!(processes.is_empty());
    Ok(())
}

/// Regression: a helper too slow to exit ended the broker.
///
/// `fork` kills and reaps a helper that misses the release deadline and then
/// reports `TimedOut`, although the pin is released and the group untouched.
/// Propagating that error made the broker exit, and every service of the
/// supervisor died with it.
#[test]
fn a_timed_out_release_whose_helper_was_reaped_is_not_fatal() -> Result<(), Box<dyn Error>> {
    let mut processes = BTreeMap::new();
    let release = release_guard_with(
        generation()?,
        helper()?,
        &mut processes,
        || Err(io::Error::new(io::ErrorKind::TimedOut, "helper too slow")),
        |_| Err(io::Error::from_raw_os_error(libc::ECHILD)),
    )?;
    assert_eq!(release, GuardRelease::Failed);
    assert!(processes.is_empty());
    Ok(())
}

/// A helper that is still an unreaped child is collected later, never lost.
#[test]
fn a_failed_release_with_a_helper_still_exiting_is_collected_later() -> Result<(), Box<dyn Error>> {
    let generation = generation()?;
    let helper = helper()?;
    for probed in [
        None,
        Some(ChildEvent::Stopped {
            pid: helper,
            signal: 19,
        }),
        Some(ChildEvent::Continued { pid: helper }),
    ] {
        let mut processes = BTreeMap::new();
        let release = release_guard_with(
            generation,
            helper,
            &mut processes,
            || Err(io::Error::from(io::ErrorKind::BrokenPipe)),
            |_| Ok(probed),
        )?;
        assert_eq!(release, GuardRelease::Failed);
        assert_eq!(
            processes.get(&helper).copied(),
            Some(BrokerOwnedProcess::ReleasedGuard(generation)),
            "{probed:?} leaves the helper to collect"
        );
    }
    Ok(())
}

#[test]
fn a_failed_release_whose_probe_reaps_the_helper_records_nothing() -> Result<(), Box<dyn Error>> {
    let helper = helper()?;
    let mut processes = BTreeMap::new();
    let release = release_guard_with(
        generation()?,
        helper,
        &mut processes,
        || Err(io::Error::from(io::ErrorKind::WouldBlock)),
        |_| {
            Ok(Some(ChildEvent::Signaled {
                pid: helper,
                signal: 9,
            }))
        },
    )?;
    assert_eq!(release, GuardRelease::Failed);
    assert!(processes.is_empty());
    Ok(())
}

/// A violated waitable-child policy breaks ownership itself and stays fatal.
#[test]
fn a_waitable_child_policy_violation_stays_fatal() -> Result<(), Box<dyn Error>> {
    let mut processes = BTreeMap::new();
    let probed = Cell::new(false);
    let release = release_guard_with(
        generation()?,
        helper()?,
        &mut processes,
        || Err(io::Error::new(io::ErrorKind::InvalidInput, "auto-reaping")),
        |_| {
            probed.set(true);
            Ok(None)
        },
    );
    assert!(release.is_err());
    assert!(!probed.get());
    assert!(processes.is_empty());
    Ok(())
}

/// A probe that cannot tell whether the helper is gone stays fatal.
#[test]
fn a_failed_helper_probe_stays_fatal() -> Result<(), Box<dyn Error>> {
    let mut processes = BTreeMap::new();
    let release = release_guard_with(
        generation()?,
        helper()?,
        &mut processes,
        || Err(io::Error::from(io::ErrorKind::TimedOut)),
        |_| Err(io::Error::from_raw_os_error(libc::EINVAL)),
    );
    assert!(release.is_err());
    assert!(processes.is_empty());
    Ok(())
}

/// A released helper's later state changes are never workload events.
#[test]
fn a_released_guard_reap_is_never_a_workload_event() -> Result<(), Box<dyn Error>> {
    let generation = generation()?;
    let helper = helper()?;
    let mut generations = BTreeMap::new();
    let mut processes = BTreeMap::new();
    let mut logging = BrokerLogging::prepare(BrokerLoggingPlan {
        local_files: Vec::new(),
        logger: None,
    })?;
    for event in [
        ChildEvent::Stopped {
            pid: helper,
            signal: 19,
        },
        ChildEvent::Continued { pid: helper },
        ChildEvent::Exited {
            pid: helper,
            code: 0,
        },
        ChildEvent::Signaled {
            pid: helper,
            signal: 9,
        },
    ] {
        let forwarded = correlate_owned_event(
            event,
            BrokerOwnedProcess::ReleasedGuard(generation),
            &mut generations,
            &mut processes,
            &mut logging,
            &mut RetiringGroups::default(),
            None,
        )?;
        assert!(forwarded.is_none(), "{event:?} must forward no event");
    }
    Ok(())
}

/// A guard closed fail-closed whose helper is still not collected is recorded
/// too, so its later reap is never an unowned child.
#[test]
fn a_closed_guard_helper_still_exiting_is_collected_later() -> Result<(), Box<dyn Error>> {
    let generation = generation()?;
    let helper = helper()?;
    let mut processes = BTreeMap::new();
    assert!(collect_released_helper_with(
        generation,
        helper,
        &mut processes,
        |_| Ok(None)
    )?);
    assert_eq!(
        processes.get(&helper).copied(),
        Some(BrokerOwnedProcess::ReleasedGuard(generation))
    );
    let mut processes = BTreeMap::new();
    assert!(!collect_released_helper_with(
        generation,
        helper,
        &mut processes,
        |_| Err(io::Error::from_raw_os_error(libc::ECHILD))
    )?);
    assert!(processes.is_empty());
    Ok(())
}
