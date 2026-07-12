//! Pre-fork account database resolution for child credential transitions.

use std::{collections::BTreeSet, ffi::CString, io};

use nix::unistd::{Gid, User, getgrouplist};

const MAX_GROUPS: usize = 65_536;

/// Numeric account data safe to carry through the process-broker protocol.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccountIdentity {
    pub(crate) user: libc::uid_t,
    pub(crate) group: libc::gid_t,
    pub(crate) supplementary_groups: Vec<libc::gid_t>,
}

/// Resolve one account name and its complete group list before any fork.
pub(crate) fn resolve(name: &str) -> io::Result<AccountIdentity> {
    let user = User::from_name(name).map_err(errno_to_io)?.ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, "configured account does not exist")
    })?;
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "account name must not contain NUL",
        )
    })?;
    let groups = getgrouplist(&c_name, user.gid).map_err(errno_to_io)?;
    if groups.len() > MAX_GROUPS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "account group count exceeds the broker limit",
        ));
    }
    let mut seen = BTreeSet::new();
    let supplementary_groups = groups
        .into_iter()
        .map(Gid::as_raw)
        .filter(|group| seen.insert(*group))
        .collect();
    Ok(AccountIdentity {
        user: user.uid.as_raw(),
        group: user.gid.as_raw(),
        supplementary_groups,
    })
}

fn errno_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}
