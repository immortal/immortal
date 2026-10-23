//! Process groups the broker killed and must see drain before it exits.
//!
//! When the broker kills a generation's group it removes the generation at
//! once, but a killed member may still be exiting, or linger as a zombie, for a
//! moment afterwards. [`RetiringGroups`] remembers those groups so ordered
//! shutdown and supervisor-loss cleanup can wait until each one has no member
//! left instead of exiting and letting the stragglers reparent to the
//! supervisor. Membership is checked with the null-signal
//! [`process_group_exists`] probe, which counts zombies, so a group drains once
//! the broker, or another parent, has reaped every member.
//!
//! Safety invariants:
//!
//! - Only groups the broker itself killed, including on a supervisor request,
//!   plus groups it stopped owning without a pin (a completed tracked
//!   generation's group and a descriptor group whose guard exited), are
//!   retired.
//! - The set offers no iteration, and no explicit group signal, a supervisor
//!   request included, reaches a group it [`contains`](RetiringGroups::contains): once
//!   a group may have emptied, its numeric ID can belong to an unrelated group.
//!   A retiring generation's unreaped child can still be signalled as a
//!   process, which its PID pins. A still-armed guard may instead be closed
//!   fail-closed: its helper's owner-loss kill remains protected by its anchor.
//! - A probe that reports a reused ID as present, or fails, only keeps the
//!   group retiring; the bounded caller then fails closed rather than finishing
//!   early.

use std::collections::BTreeSet;
use std::io;

use super::{ProcessGroupId, process_group_exists};

/// Killed process groups still waiting to drain.
///
/// Storage is one ordered set of group IDs, so retiring, forgetting, and
/// pruning are logarithmic and allocate only per retired group.
#[derive(Debug, Default)]
pub(super) struct RetiringGroups {
    groups: BTreeSet<ProcessGroupId>,
    last_probe_error: Option<i32>,
}

impl RetiringGroups {
    /// Remember a group the broker killed so shutdown waits for it to drain.
    pub(super) fn retire(&mut self, group: ProcessGroupId) {
        self.groups.insert(group);
    }

    /// Stop waiting for `group`, whose ID now provably names a different group.
    ///
    /// A new process can lead a group with this ID only after every member of
    /// the old group was reaped, so the retiring entry is already satisfied.
    pub(super) fn forget(&mut self, group: ProcessGroupId) {
        self.groups.remove(&group);
    }

    /// Whether `group` is retiring, so no explicit group request may signal it again.
    pub(super) fn contains(&self, group: ProcessGroupId) -> bool {
        self.groups.contains(&group)
    }

    /// Whether every retired group has drained.
    pub(super) fn is_empty(&self) -> bool {
        self.groups.is_empty()
    }

    /// The OS error from the most recent prune pass, if any probe failed.
    pub(super) const fn last_probe_error(&self) -> Option<i32> {
        self.last_probe_error
    }

    /// Release every retired group the probe reports as drained.
    pub(super) fn prune(&mut self) {
        self.prune_with(process_group_exists);
    }

    /// Release drained groups using `probe`, keeping any group it cannot clear.
    ///
    /// A present group and a probe error both keep the group retiring, so the
    /// caller fails closed; the pass records the last OS error it saw and
    /// clears any error left from an earlier pass.
    fn prune_with<P>(&mut self, mut probe: P)
    where
        P: FnMut(ProcessGroupId) -> io::Result<bool>,
    {
        let mut last_error = None;
        self.groups.retain(|group| match probe(*group) {
            Ok(present) => present,
            Err(error) => {
                last_error = error.raw_os_error().or(last_error);
                true
            }
        });
        self.last_probe_error = last_error;
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::error::Error;
    use std::io;

    use super::{ProcessGroupId, RetiringGroups};

    fn group(raw: i32) -> Result<ProcessGroupId, Box<dyn Error>> {
        Ok(ProcessGroupId::try_from(raw)?)
    }

    #[test]
    fn prune_releases_only_groups_the_probe_reports_absent() -> Result<(), Box<dyn Error>> {
        let drained = group(4_242)?;
        let occupied = group(4_343)?;
        let mut retiring = RetiringGroups::default();
        retiring.retire(drained);
        retiring.retire(occupied);

        retiring.prune_with(|candidate| Ok(candidate == occupied));
        assert!(!retiring.is_empty(), "an occupied group must keep retiring");
        assert!(retiring.contains(occupied) && !retiring.contains(drained));

        retiring.prune_with(|_| Ok(false));
        assert!(retiring.is_empty());
        assert_eq!(retiring.last_probe_error(), None);
        Ok(())
    }

    #[test]
    fn prune_keeps_a_group_whose_probe_fails_and_reports_the_error() -> Result<(), Box<dyn Error>> {
        let mut retiring = RetiringGroups::default();
        retiring.retire(group(4_242)?);

        retiring.prune_with(|_| Err(io::Error::from_raw_os_error(libc::EINVAL)));
        assert!(!retiring.is_empty(), "a failed probe must fail closed");
        assert_eq!(retiring.last_probe_error(), Some(libc::EINVAL));

        retiring.prune_with(|_| Ok(false));
        assert!(retiring.is_empty());
        assert_eq!(
            retiring.last_probe_error(),
            None,
            "a clean pass clears the stale error"
        );
        Ok(())
    }

    #[test]
    fn forget_releases_only_the_named_group() -> Result<(), Box<dyn Error>> {
        let reused = group(4_242)?;
        let other = group(4_343)?;
        let mut retiring = RetiringGroups::default();
        retiring.retire(reused);
        retiring.retire(reused);
        retiring.retire(other);

        retiring.forget(reused);
        assert!(!retiring.contains(reused));
        assert!(retiring.contains(other));
        retiring.prune_with(|candidate| Ok(candidate == other));
        assert!(!retiring.is_empty());
        retiring.forget(other);
        assert!(retiring.is_empty());
        Ok(())
    }

    #[test]
    fn an_empty_set_never_probes() {
        let probes = Cell::new(0_u32);
        let mut retiring = RetiringGroups::default();
        retiring.prune_with(|_| {
            probes.set(probes.get() + 1);
            Ok(true)
        });
        assert_eq!(probes.get(), 0);
        assert!(retiring.is_empty());
    }
}
