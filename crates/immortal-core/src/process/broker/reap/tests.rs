use std::error::Error;
use std::io;

use super::super::SpawnedProcess;
use super::super::logging::BrokerLoggingPlan;
use super::super::state::BrokerOwnedProcess;
use super::{
    BTreeMap, BrokerEvent, BrokerLogging, ChildEvent, GroupPin, GroupRecord, ProcessGroupId,
    ProcessId, RetiringGroups, SignalTarget, correlate_owned_event, kill_and_retire_with,
    kill_live_group_with, pinned_by_another_with, retire_completed_group, signal_live_group_with,
    signal_owned_target_with,
};
use crate::supervisor::Generation;

/// Armed group requests never cross the direct numeric-ID boundary, on either
/// successful delivery or helper loss. Process targets still use the direct path.
#[test]
fn armed_group_signals_never_use_direct_delivery() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    for helper_gone in [false, true] {
        let mut guarded = 0;
        let mut direct = Vec::new();
        let result = signal_owned_target_with(
            SignalTarget::Group(group),
            true,
            || {
                guarded += 1;
                if helper_gone {
                    Err(io::Error::from(io::ErrorKind::BrokenPipe))
                } else {
                    Ok(())
                }
            },
            |target| {
                direct.push(target);
                Ok(())
            },
        );
        assert_eq!(guarded, 1);
        assert_eq!(direct, []);
        assert_eq!(result.is_err(), helper_gone);
    }
    for (target, armed) in [
        (SignalTarget::Process(process), true),
        (SignalTarget::Group(group), false),
    ] {
        let mut guarded = false;
        let mut direct = Vec::new();
        signal_owned_target_with(
            target,
            armed,
            || {
                guarded = true;
                Ok(())
            },
            |target| {
                direct.push(target);
                Ok(())
            },
        )?;
        assert!(!guarded);
        assert_eq!(direct, [target]);
    }
    Ok(())
}

/// Losing the helper still retires a killed group, but never retries directly.
#[test]
fn helper_gone_kill_retires_without_direct_delivery() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let mut retiring = RetiringGroups::default();
    let mut direct = Vec::new();
    let result = kill_live_group_with(
        process,
        group,
        ARMED,
        &mut retiring,
        |_| Ok(group),
        |target| {
            signal_owned_target_with(
                target,
                true,
                || Err(io::Error::from(io::ErrorKind::BrokenPipe)),
                |target| {
                    direct.push(target);
                    Ok(())
                },
            )
        },
    );
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::BrokenPipe));
    assert_eq!(direct, []);
    assert!(retiring.contains(group));
    Ok(())
}

/// A spent phase never queues another helper request; skipped kills still retire.
#[test]
fn expired_signal_phase_skips_delivery_and_retires_kills() -> Result<(), Box<dyn Error>> {
    let (_, group) = process_and_group()?;
    let mut retiring = RetiringGroups::default();
    let mut attempted = false;
    let deadline = std::time::Instant::now();
    let result = kill_and_retire_with(group, &mut retiring, |_| {
        super::with_helper_timeout(Some(deadline), |_| {
            attempted = true;
            Ok(())
        })
    });
    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::TimedOut));
    assert!(!attempted);
    assert!(retiring.contains(group));
    Ok(())
}

/// The cap applies outside phases and is reduced by a phase's remaining time.
#[test]
fn helper_round_trips_share_the_remaining_phase_budget() -> io::Result<()> {
    let short = std::time::Duration::from_millis(100);
    let deadline = std::time::Instant::now() + short;
    super::with_helper_timeout(Some(deadline), |timeout| {
        assert!(!timeout.is_zero() && timeout <= short);
        Ok(())
    })?;
    super::with_helper_timeout(None, |timeout| {
        assert_eq!(timeout, super::GROUP_SIGNAL_TIMEOUT);
        Ok(())
    })
}

/// Only delivery success or ESRCH permits releasing containment intentionally.
/// Every other outcome keeps the group retired and selects owner-loss cleanup.
#[test]
fn foreground_guard_disarms_only_after_delivery_or_esrch() -> Result<(), Box<dyn Error>> {
    let (_, group) = process_and_group()?;
    for (delivery, expected, retired) in [
        (Ok(()), true, true),
        (Err(io::Error::from_raw_os_error(libc::ESRCH)), true, false),
        (Err(io::Error::from(io::ErrorKind::TimedOut)), false, true),
        (Err(io::Error::from(io::ErrorKind::BrokenPipe)), false, true),
        (Err(io::Error::from_raw_os_error(libc::EPERM)), false, true),
        (Err(io::Error::from_raw_os_error(libc::EIO)), false, true),
    ] {
        let mut retiring = RetiringGroups::default();
        let disarm = super::foreground_kill_allows_disarm_with(false, || {
            kill_and_retire_with(group, &mut retiring, |_| delivery)
        });
        assert_eq!(disarm, expected);
        assert_eq!(retiring.contains(group), retired);
    }
    Ok(())
}

/// A previous failed kill can have retired the group. The no-op success from
/// kill-and-retire is not an acknowledgment and cannot authorize disarm.
#[test]
fn foreground_retired_group_keeps_fail_closed_cleanup_without_resignalling()
-> Result<(), Box<dyn Error>> {
    let (_, group) = process_and_group()?;
    let mut retiring = RetiringGroups::default();
    let failed = kill_and_retire_with(group, &mut retiring, |_| {
        Err(io::Error::from(io::ErrorKind::TimedOut))
    });
    assert!(failed.is_err());
    let mut attempted = false;
    let disarm = super::foreground_kill_allows_disarm_with(retiring.contains(group), || {
        attempted = true;
        kill_and_retire_with(group, &mut retiring, |_| Ok(()))
    });
    assert!(!disarm && !attempted);
    assert!(retiring.contains(group));
    Ok(())
}

const GUARD_PID: i32 = 4_242;
/// No guard and no other generation pins the group.
const UNPINNED: GroupPin = GroupPin {
    armed: false,
    reused: false,
};
/// The generation's armed guard pins the group.
const ARMED: GroupPin = GroupPin {
    armed: true,
    reused: false,
};
/// No guard pins the group and another generation now pins its ID.
const REUSED: GroupPin = GroupPin {
    armed: false,
    reused: true,
};

/// Once a group is retiring its ID may drain and be reused, so a second
/// kill, such as a foreground reap after supervisor loss already killed the
/// group, must never reach the operating system.
#[test]
fn kill_and_retire_never_signals_a_retired_group() -> Result<(), Box<dyn Error>> {
    let group = ProcessGroupId::try_from(GUARD_PID)?;
    let mut retiring = RetiringGroups::default();
    retiring.retire(group);
    let mut sent = false;
    kill_and_retire_with(group, &mut retiring, |_| {
        sent = true;
        Ok(())
    })?;
    assert!(!sent, "a retired group was signalled again");
    assert!(retiring.contains(group));
    Ok(())
}

fn process_and_group() -> Result<(ProcessId, ProcessGroupId), Box<dyn Error>> {
    Ok((guard_process()?, ProcessGroupId::try_from(GUARD_PID)?))
}

/// An unreaped member leader reserves the group ID, so the group itself is signalled, and
/// only once, whether or not the guard pins it too.
#[test]
fn a_member_leader_signals_its_whole_group() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    for pin in [UNPINNED, ARMED] {
        let mut retiring = RetiringGroups::default();
        let mut sent = Vec::new();
        signal_live_group_with(
            process,
            group,
            pin,
            &mut retiring,
            |_| Ok(group),
            |target| {
                sent.push(target);
                Ok(())
            },
        )?;
        assert_eq!(sent, [SignalTarget::Group(group)], "pin {pin:?}");
        assert!(!retiring.contains(group), "a terminate retired the group");
    }
    Ok(())
}

/// An armed guard pins its group even after the child left it, so the group
/// is signalled and the departed child is signalled as a process too; a kill
/// retires the group, and once it is retiring only the child is signalled.
#[test]
fn an_armed_group_is_signalled_after_its_child_left() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let elsewhere = ProcessGroupId::try_from(GUARD_PID + 1)?;
    for query in [Ok(elsewhere), Err(libc::ESRCH)] {
        let mut retiring = RetiringGroups::default();
        let mut sent = Vec::new();
        signal_live_group_with(
            process,
            group,
            ARMED,
            &mut retiring,
            |_| query.map_err(io::Error::from_raw_os_error),
            |target| {
                sent.push(target);
                Ok(())
            },
        )?;
        assert_eq!(
            sent,
            [SignalTarget::Group(group), SignalTarget::Process(process)],
            "query {query:?}"
        );
        assert!(!retiring.contains(group), "a terminate retired the group");

        let mut killed = Vec::new();
        kill_live_group_with(
            process,
            group,
            ARMED,
            &mut retiring,
            |_| query.map_err(io::Error::from_raw_os_error),
            |target| {
                killed.push(target);
                Ok(())
            },
        )?;
        assert_eq!(
            killed,
            [SignalTarget::Group(group), SignalTarget::Process(process)]
        );
        assert!(retiring.contains(group), "the killed group is not awaited");

        let mut repeated = Vec::new();
        signal_live_group_with(
            process,
            group,
            ARMED,
            &mut retiring,
            |_| query.map_err(io::Error::from_raw_os_error),
            |target| {
                repeated.push(target);
                Ok(())
            },
        )?;
        assert_eq!(repeated, [SignalTarget::Process(process)]);
    }
    Ok(())
}

/// Once the child left its armed group, its own delivery answers the request:
/// the group signal still goes out first for members left behind, but its
/// failure, such as macOS's `EPERM` for a group holding only the anchor, is not
/// reported, while a failed delivery to the child is. A failed group kill
/// still retires the group.
#[test]
fn a_departed_childs_delivery_answers_an_armed_group_signal() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let elsewhere = ProcessGroupId::try_from(GUARD_PID + 1)?;
    let outcome = |target: SignalTarget, group_errno: Option<i32>, child_errno: Option<i32>| {
        let errno = match target {
            SignalTarget::Group(_) => group_errno,
            SignalTarget::Process(_) => child_errno,
        };
        errno.map_or(Ok(()), |errno| Err(io::Error::from_raw_os_error(errno)))
    };
    for (group_errno, child_errno, expected) in [
        (Some(libc::EPERM), None, None),
        (None, Some(libc::ESRCH), Some(libc::ESRCH)),
    ] {
        let mut retiring = RetiringGroups::default();
        let mut sent = Vec::new();
        let terminated = signal_live_group_with(
            process,
            group,
            ARMED,
            &mut retiring,
            |_| Ok(elsewhere),
            |target| {
                sent.push(target);
                outcome(target, group_errno, child_errno)
            },
        );
        assert_eq!(
            terminated.err().and_then(|error| error.raw_os_error()),
            expected
        );
        let killed = kill_live_group_with(
            process,
            group,
            ARMED,
            &mut retiring,
            |_| Ok(elsewhere),
            |target| {
                sent.push(target);
                outcome(target, group_errno, child_errno)
            },
        );
        assert_eq!(
            killed.err().and_then(|error| error.raw_os_error()),
            expected
        );
        assert!(retiring.contains(group), "the killed group is not awaited");
        assert_eq!(
            sent,
            [
                SignalTarget::Group(group),
                SignalTarget::Process(process),
                SignalTarget::Group(group),
                SignalTarget::Process(process),
            ]
        );
    }
    Ok(())
}

/// Without an armed guard, a child which left its group, or whose group the
/// platform will not report, no longer pins it, so only the child itself is
/// signalled, and the group it left is retired so no later kill can reach it.
#[test]
fn a_child_outside_its_group_is_signalled_alone() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let elsewhere = ProcessGroupId::try_from(GUARD_PID + 1)?;
    for query in [Ok(elsewhere), Err(libc::ESRCH)] {
        let mut retiring = RetiringGroups::default();
        let mut sent = Vec::new();
        kill_live_group_with(
            process,
            group,
            UNPINNED,
            &mut retiring,
            |_| query.map_err(io::Error::from_raw_os_error),
            |target| {
                sent.push(target);
                Ok(())
            },
        )?;
        assert_eq!(sent, [SignalTarget::Process(process)], "query {query:?}");
        assert!(retiring.contains(group), "the left group is still awaited");

        let mut retiring = RetiringGroups::default();
        let mut terminated = Vec::new();
        signal_live_group_with(
            process,
            group,
            UNPINNED,
            &mut retiring,
            |_| query.map_err(io::Error::from_raw_os_error),
            |target| {
                terminated.push(target);
                Ok(())
            },
        )?;
        assert_eq!(terminated, [SignalTarget::Process(process)]);
        assert!(
            retiring.contains(group),
            "a later kill must skip the left group"
        );
        let mut killed = false;
        kill_and_retire_with(group, &mut retiring, |_| {
            killed = true;
            Ok(())
        })?;
        assert!(
            !killed,
            "the reap's kill reached a group the child had left"
        );
    }
    Ok(())
}

/// A pinned group is killed and retired once; a retiring one is skipped.
#[test]
fn a_member_leader_kills_and_retires_its_group_once() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let mut retiring = RetiringGroups::default();
    let mut sent = Vec::new();
    for _ in 0..2 {
        kill_live_group_with(
            process,
            group,
            UNPINNED,
            &mut retiring,
            |_| Ok(group),
            |target| {
                sent.push(target);
                Ok(())
            },
        )?;
    }
    assert_eq!(sent, [SignalTarget::Group(group)]);
    assert!(retiring.contains(group));
    let mut terminated = Vec::new();
    signal_live_group_with(
        process,
        group,
        UNPINNED,
        &mut retiring,
        |_| Ok(group),
        |target| {
            terminated.push(target);
            Ok(())
        },
    )?;
    assert!(
        terminated.is_empty(),
        "a retiring group was signalled again"
    );
    Ok(())
}

/// A group ID another live generation pins, through its armed guard or a
/// member leader, names that generation's group, so a retire or kill on behalf
/// of a different owner must skip it; the owner itself, or a generation that
/// merely records the ID without either pin, does not count.
#[test]
fn a_reused_group_is_recognised_only_through_another_pin() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let elsewhere = ProcessGroupId::try_from(GUARD_PID + 1)?;
    let owner = Generation::new(1).ok_or("invalid owner generation")?;
    let other = Generation::new(2).ok_or("invalid other generation")?;
    let record = |generation, armed, child| GroupRecord {
        generation,
        group,
        armed,
        child,
    };
    let member = Some(SpawnedProcess::for_test(process, group));
    let pinned = |records: &[GroupRecord], owner, current| {
        pinned_by_another_with(records.iter().copied(), owner, group, |_| Ok(current))
    };

    let unpinned = [record(owner, true, member), record(other, false, None)];
    assert!(!pinned(&unpinned, owner, group));
    let by_child = [record(owner, false, None), record(other, false, member)];
    assert!(pinned(&by_child, owner, group));
    assert!(
        !pinned(&by_child, owner, elsewhere),
        "a departed child pins nothing"
    );
    assert!(!pinned(&by_child, other, group), "the owner never counts");
    let by_guard = [record(owner, false, None), record(other, true, None)];
    assert!(pinned(&by_guard, owner, elsewhere));
    let unrelated = [GroupRecord {
        group: elsewhere,
        ..record(other, true, member)
    }];
    assert!(!pinned(&unrelated, owner, group), "another ID never counts");
    Ok(())
}

/// A child outside its group whose ID another generation now pins is
/// signalled alone, and the ID is not retired on its behalf.
#[test]
fn a_reused_group_is_never_retired_for_a_departed_child() -> Result<(), Box<dyn Error>> {
    let (process, group) = process_and_group()?;
    let elsewhere = ProcessGroupId::try_from(GUARD_PID + 1)?;
    let mut retiring = RetiringGroups::default();
    let mut sent = Vec::new();
    kill_live_group_with(
        process,
        group,
        REUSED,
        &mut retiring,
        |_| Ok(elsewhere),
        |target| {
            sent.push(target);
            Ok(())
        },
    )?;
    signal_live_group_with(
        process,
        group,
        REUSED,
        &mut retiring,
        |_| Ok(elsewhere),
        |target| {
            sent.push(target);
            Ok(())
        },
    )?;
    assert_eq!(
        sent,
        [
            SignalTarget::Process(process),
            SignalTarget::Process(process)
        ]
    );
    assert!(
        !retiring.contains(group),
        "another generation's live group was retired"
    );
    Ok(())
}

/// A completed tracked group is awaited without a signal, unless its guard
/// already exited and retired it, since its ID may have been reused since.
#[test]
fn a_completed_tracked_group_is_retired_only_while_its_guard_is_armed() -> Result<(), Box<dyn Error>>
{
    let group = ProcessGroupId::try_from(GUARD_PID)?;
    let mut retiring = RetiringGroups::default();
    retire_completed_group(group, false, &mut retiring);
    assert!(
        !retiring.contains(group),
        "a disarmed group was retired again"
    );
    retire_completed_group(group, true, &mut retiring);
    assert!(retiring.contains(group));
    Ok(())
}

/// Only an empty group (`ESRCH`) skips retirement; a delivered kill or any
/// other failure keeps the group awaited so shutdown fails closed.
#[test]
fn kill_and_retire_retires_unless_the_group_is_already_empty() -> Result<(), Box<dyn Error>> {
    let group = ProcessGroupId::try_from(GUARD_PID)?;
    for (outcome, retired) in [
        (Ok(()), true),
        (Err(libc::EPERM), true),
        (Err(libc::ESRCH), false),
    ] {
        let mut retiring = RetiringGroups::default();
        let result = kill_and_retire_with(group, &mut retiring, |_| {
            outcome.map_err(io::Error::from_raw_os_error)
        });
        assert_eq!(result.is_ok(), outcome.is_ok());
        assert_eq!(retiring.contains(group), retired, "outcome {outcome:?}");
    }
    Ok(())
}

fn guard_process() -> Result<ProcessId, Box<dyn Error>> {
    ProcessId::new(GUARD_PID).ok_or_else(|| "invalid test process id".into())
}

fn test_generation() -> Result<Generation, Box<dyn Error>> {
    Generation::new(1).ok_or_else(|| "invalid test generation".into())
}

fn empty_logging() -> Result<BrokerLogging, Box<dyn Error>> {
    Ok(BrokerLogging::prepare(BrokerLoggingPlan {
        local_files: Vec::new(),
        logger: None,
    })?)
}

/// Regression: a suspended guard is not a containment failure.
///
/// Waits report job-control stops and resumes alongside exits. The guard
/// arm acted on every event, so delivering `SIGSTOP` to the guard killed
/// the workload group and reported containment failure, which the executor
/// escalates into a supervisor abort.
#[test]
fn a_stopped_guard_is_not_a_containment_failure() -> Result<(), Box<dyn Error>> {
    let generation = test_generation()?;
    let process = guard_process()?;
    let mut generations = BTreeMap::new();
    let mut processes = BTreeMap::from([(process, BrokerOwnedProcess::Guard(generation))]);
    let mut logging = empty_logging()?;

    for event in [
        ChildEvent::Stopped {
            pid: process,
            signal: 19,
        },
        ChildEvent::Continued { pid: process },
    ] {
        let forwarded = correlate_owned_event(
            event,
            BrokerOwnedProcess::Guard(generation),
            &mut generations,
            &mut processes,
            &mut logging,
            &mut RetiringGroups::default(),
            None,
        )?;
        assert!(forwarded.is_none(), "{event:?} must forward no event");
    }

    // A stopped guard remains owned and waitable, so its ownership entry
    // must survive for the eventual terminal event to correlate against.
    assert_eq!(
        processes.get(&process).copied(),
        Some(BrokerOwnedProcess::Guard(generation))
    );
    Ok(())
}

/// A guard that exits still fails containment closed.
#[test]
fn an_exited_guard_reports_containment_failure() -> Result<(), Box<dyn Error>> {
    let generation = test_generation()?;
    let process = guard_process()?;
    let mut generations = BTreeMap::new();
    let mut processes = BTreeMap::new();
    let mut logging = empty_logging()?;

    let forwarded = correlate_owned_event(
        ChildEvent::Exited {
            pid: process,
            code: 0,
        },
        BrokerOwnedProcess::Guard(generation),
        &mut generations,
        &mut processes,
        &mut logging,
        &mut RetiringGroups::default(),
        None,
    )?;
    assert_eq!(
        forwarded,
        Some(BrokerEvent::ContainmentFailed { generation })
    );
    Ok(())
}

/// A signalled guard is terminal too, and must not be mistaken for a stop.
#[test]
fn a_signalled_guard_reports_containment_failure() -> Result<(), Box<dyn Error>> {
    let generation = test_generation()?;
    let process = guard_process()?;
    let mut generations = BTreeMap::new();
    let mut processes = BTreeMap::new();
    let mut logging = empty_logging()?;

    let forwarded = correlate_owned_event(
        ChildEvent::Signaled {
            pid: process,
            signal: 9,
        },
        BrokerOwnedProcess::Guard(generation),
        &mut generations,
        &mut processes,
        &mut logging,
        &mut RetiringGroups::default(),
        None,
    )?;
    assert_eq!(
        forwarded,
        Some(BrokerEvent::ContainmentFailed { generation })
    );
    Ok(())
}

/// Even a successful membership observation cannot pin a live nonleader.
#[test]
fn a_nonleader_membership_snapshot_never_authorizes_direct_group_delivery()
-> Result<(), Box<dyn Error>> {
    let (_, group) = process_and_group()?;
    let process = ProcessId::try_from(GUARD_PID + 1)?;
    for kill in [false, true] {
        let mut retiring = RetiringGroups::default();
        let mut sent = Vec::new();
        let query = |_| Ok(group);
        let deliver = |target| {
            sent.push(target);
            Ok(())
        };
        if kill {
            kill_live_group_with(process, group, UNPINNED, &mut retiring, query, deliver)?;
        } else {
            signal_live_group_with(process, group, UNPINNED, &mut retiring, query, deliver)?;
        }
        assert_eq!(sent, [SignalTarget::Process(process)]);
        assert!(retiring.contains(group));
    }
    Ok(())
}

#[test]
fn another_nonleader_membership_snapshot_is_not_a_pin() -> Result<(), Box<dyn Error>> {
    let (_, group) = process_and_group()?;
    let process = ProcessId::try_from(GUARD_PID + 1)?;
    let owner = Generation::FIRST;
    let other = Generation::new(2).ok_or("invalid generation")?;
    let records = [GroupRecord {
        generation: other,
        group,
        armed: false,
        child: Some(SpawnedProcess::for_test(process, group)),
    }];
    assert!(!pinned_by_another_with(
        records.into_iter(),
        owner,
        group,
        |_| Ok(group)
    ));
    Ok(())
}
