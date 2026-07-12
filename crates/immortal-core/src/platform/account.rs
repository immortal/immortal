//! Pre-fork account database resolution for child credential transitions.
//!
//! Linux and FreeBSD use the reentrant account interfaces exposed by `nix`.
//! Apple excludes `getgrouplist` because Open Directory is authoritative, so
//! macOS executes the absolute system `id` utility without a shell and accepts
//! only bounded numeric output. All lookup processes finish before daemonization,
//! broker creation, or Tokio, and only resolved numeric identities cross IPC.

use std::{collections::BTreeSet, io};

#[cfg(not(target_os = "macos"))]
use std::ffi::CString;
#[cfg(target_os = "macos")]
use std::process::Command;

#[cfg(not(target_os = "macos"))]
use nix::unistd::getgrouplist;
use nix::unistd::{Gid, User};

const MAX_GROUPS: usize = 65_536;
#[cfg(any(test, target_os = "macos"))]
const MAX_GROUP_OUTPUT_BYTES: usize = MAX_GROUPS * 16;

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
    let supplementary_groups = supplementary_groups(name, user.gid)?;
    Ok(AccountIdentity {
        user: user.uid.as_raw(),
        group: user.gid.as_raw(),
        supplementary_groups,
    })
}

#[cfg(not(target_os = "macos"))]
fn supplementary_groups(name: &str, primary: Gid) -> io::Result<Vec<libc::gid_t>> {
    let c_name = CString::new(name).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "account name must not contain NUL",
        )
    })?;
    let groups = getgrouplist(&c_name, primary).map_err(errno_to_io)?;
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
    Ok(supplementary_groups)
}

/// Ask macOS's `id` utility so Open Directory, rather than the local group
/// database alone, remains the authority for supplementary memberships.
#[cfg(target_os = "macos")]
fn supplementary_groups(name: &str, primary: Gid) -> io::Result<Vec<libc::gid_t>> {
    let output = Command::new("/usr/bin/id")
        .args(["-G", "--", name])
        .output()?;
    if !output.status.success() {
        return Err(io::Error::other(format!(
            "macOS account group lookup exited with {}",
            output.status
        )));
    }
    parse_group_ids(&output.stdout, primary.as_raw())
}

#[cfg(any(test, target_os = "macos"))]
fn parse_group_ids(bytes: &[u8], primary: libc::gid_t) -> io::Result<Vec<libc::gid_t>> {
    if bytes.is_empty() || bytes.len() > MAX_GROUP_OUTPUT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS account group output is empty or exceeds the broker limit",
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS account group output is not UTF-8",
        )
    })?;
    let mut groups = BTreeSet::from([primary]);
    let mut observed = false;
    for token in text.split_ascii_whitespace() {
        observed = true;
        let value = token.parse::<u64>().map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "macOS account group output contains a nonnumeric group",
            )
        })?;
        let group = libc::gid_t::try_from(value).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "macOS account group output contains an out-of-range group",
            )
        })?;
        groups.insert(group);
        if groups.len() > MAX_GROUPS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "account group count exceeds the broker limit",
            ));
        }
    }
    if !observed {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "macOS account group output contains no groups",
        ));
    }
    Ok(groups.into_iter().collect())
}

fn errno_to_io(error: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(error as i32)
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io};

    use super::{MAX_GROUP_OUTPUT_BYTES, parse_group_ids};

    #[test]
    fn macos_group_output_deduplicates_and_includes_primary() -> Result<(), Box<dyn Error>> {
        let groups = parse_group_ids(b"20 10 20\n", 30)?;
        assert_eq!(groups, [10, 20, 30]);
        Ok(())
    }

    #[test]
    fn macos_group_output_rejects_malformed_and_unbounded_data() {
        for input in [b"".as_slice(), b" \n", b"10 invalid", b"\xff"] {
            assert!(
                parse_group_ids(input, 10)
                    .is_err_and(|error| error.kind() == io::ErrorKind::InvalidData)
            );
        }
        let oversized = vec![b'1'; MAX_GROUP_OUTPUT_BYTES + 1];
        assert!(
            parse_group_ids(&oversized, 10)
                .is_err_and(|error| error.kind() == io::ErrorKind::InvalidData)
        );
    }
}
