//! Durable normalized definition snapshots and deletion-confirmation ledger.
//!
//! Applied snapshots remain the configuration authority while the tracker ledger
//! stores only safe service names and bounded absence counts. Every read and
//! write validates ownership, file type, permissions, size, and replacement
//! identity so persisted reconciliation state cannot be redirected or widened by
//! filesystem races.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as FmtWrite,
    fs::{self, DirBuilder, File, Metadata, OpenOptions},
    io::{self, Read, Write},
    num::NonZeroUsize,
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    limits::{DEFAULT_DELETION_CONFIRMATIONS, DEFAULT_MAX_DEFINITIONS},
    scan::metadata_changed,
    tracker::DesiredStateTracker,
};
use crate::{
    config::{MAX_CONFIG_BYTES, ServiceConfig, emit_config, parse_bytes_at},
    service_name::is_safe_service_name,
};

const SNAPSHOT_DIRECTORY: &str = ".definitions";
pub(in crate::reconcile) const TRACKER_STATE_FILE: &str = "tracker.state";
const TRACKER_STATE_VERSION: usize = 1;
pub(in crate::reconcile) const MAX_TRACKER_STATE_BYTES: usize = 2 * 1024 * 1024;
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(0);

/// Secure store for immutable normalized definitions passed to new supervisors.
#[derive(Debug)]
pub struct DefinitionSnapshots {
    pub(in crate::reconcile) directory: PathBuf,
    owner_uid: u32,
}

impl DefinitionSnapshots {
    /// Open or create the owner-only hidden snapshot directory below a runtime root.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime root is unsafe or the snapshot directory
    /// is a symlink, has a different owner, or is not mode `0700`.
    pub fn open(runtime_root: &Path) -> io::Result<Self> {
        crate::runtime::discover(runtime_root).map_err(io::Error::other)?;
        let root = fs::symlink_metadata(runtime_root)?;
        let directory = runtime_root.join(SNAPSHOT_DIRECTORY);
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&directory) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != root.uid()
            || metadata.mode() & 0o777 != 0o700
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe reconciliation snapshot directory",
            ));
        }
        Ok(Self {
            directory,
            owner_uid: metadata.uid(),
        })
    }

    /// Atomically publish one normalized definition and return its stable path.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name, serialization, exclusive creation,
    /// write, sync, rename, or parent-directory sync failure.
    pub fn publish(&self, name: &str, config: &ServiceConfig) -> io::Result<PathBuf> {
        self.publish_named(name, "launch", config)
    }

    /// Persist the last desired state successfully applied by reconciliation.
    ///
    /// # Errors
    ///
    /// Returns the same validation, serialization, and atomic-write failures as
    /// [`Self::publish`].
    pub fn record_applied(&self, name: &str, config: &ServiceConfig) -> io::Result<()> {
        let _path = self.publish_named(name, "applied", config)?;
        Ok(())
    }

    /// Load the last applied desired state without following replacement links.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name, unsafe entry type/ownership/mode, or
    /// configuration parse and validation failure.
    pub fn load_applied(&self, name: &str) -> io::Result<Option<ServiceConfig>> {
        let path = self.named_path(name, "applied")?;
        let Some(bytes) = self.read_owned_file(&path, MAX_CONFIG_BYTES)? else {
            return Ok(None);
        };
        parse_bytes_at(&bytes, &path)
            .map(Some)
            .map_err(io::Error::other)
    }

    /// Remove applied state after a definition is stably deleted and halted.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsafe name or filesystem removal failure.
    pub fn remove_applied(&self, name: &str) -> io::Result<()> {
        let path = self.named_path(name, "applied")?;
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || metadata.uid() != self.owner_uid
            || metadata.mode() & 0o777 != 0o600
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe applied-state snapshot",
            ));
        }
        fs::remove_file(path)
    }

    /// Restore desired configurations and deletion confirmations after restart.
    ///
    /// Applied snapshots remain the configuration authority. The bounded tracker
    /// ledger contributes only safe service names and consecutive-absence counts;
    /// stale ledger entries without an applied snapshot are ignored.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe snapshot entries, an oversized or malformed
    /// ledger, duplicate names, unsupported versions, or invalid applied state.
    pub fn load_tracker(&self) -> io::Result<DesiredStateTracker> {
        let mut counts = self.load_tracker_counts()?;
        for name in self.applied_names()? {
            counts.entry(name).or_insert(0);
        }

        let deletion_confirmations =
            NonZeroUsize::new(DEFAULT_DELETION_CONFIRMATIONS).unwrap_or(NonZeroUsize::MIN);
        let mut tracker = DesiredStateTracker::new(deletion_confirmations);
        for (name, count) in counts {
            if count > deletion_confirmations.get() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker absence count exceeds deletion threshold",
                ));
            }
            let Some(config) = self.load_applied(&name)? else {
                continue;
            };
            tracker.desired.insert(name.clone(), config);
            if count > 0 {
                tracker.absent_scans.insert(name, count);
            }
        }
        Ok(tracker)
    }

    /// Atomically checkpoint desired names and deletion confirmations.
    ///
    /// The ledger deliberately excludes configuration contents, which remain in
    /// validated applied snapshots, and retains confirmed deletions until the
    /// caller acknowledges successful supervisor cleanup.
    ///
    /// # Errors
    ///
    /// Returns an error for serialization bounds or atomic write/sync failures.
    pub fn record_tracker(&self, tracker: &DesiredStateTracker) -> io::Result<()> {
        let mut names: BTreeSet<&str> = tracker.desired.keys().map(String::as_str).collect();
        names.extend(tracker.absent_scans.keys().map(String::as_str));
        if names.len() > DEFAULT_MAX_DEFINITIONS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "tracker service count exceeds definition limit",
            ));
        }

        let mut contents = format!("version\t{TRACKER_STATE_VERSION}\n");
        for name in names {
            let count = tracker.absent_scans.get(name).copied().unwrap_or(0);
            if count > tracker.deletion_confirmations.get() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker absence count exceeds deletion threshold",
                ));
            }
            writeln!(&mut contents, "{name}\t{count}").map_err(io::Error::other)?;
            if contents.len() > MAX_TRACKER_STATE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker state exceeds size limit",
                ));
            }
        }
        let destination = self.directory.join(TRACKER_STATE_FILE);
        self.atomic_replace(&destination, "tracker", contents.as_bytes())
    }

    fn load_tracker_counts(&self) -> io::Result<BTreeMap<String, usize>> {
        let path = self.directory.join(TRACKER_STATE_FILE);
        let Some(bytes) = self.read_owned_file(&path, MAX_TRACKER_STATE_BYTES)? else {
            return Ok(BTreeMap::new());
        };
        let contents = std::str::from_utf8(&bytes).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("tracker state is not UTF-8: {error}"),
            )
        })?;
        let mut lines = contents.lines();
        let header = format!("version\t{TRACKER_STATE_VERSION}");
        if lines.next() != Some(header.as_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported tracker state version",
            ));
        }

        let mut counts = BTreeMap::new();
        for line in lines {
            let Some((name, count)) = line.split_once('\t') else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "malformed tracker state entry",
                ));
            };
            if !is_safe_service_name(name) || count.contains('\t') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe tracker state entry",
                ));
            }
            let count = count.parse::<usize>().map_err(|error| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid tracker absence count: {error}"),
                )
            })?;
            if counts.insert(name.to_owned(), count).is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "duplicate tracker state entry",
                ));
            }
            if counts.len() > DEFAULT_MAX_DEFINITIONS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "tracker service count exceeds definition limit",
                ));
            }
        }
        Ok(counts)
    }

    fn applied_names(&self) -> io::Result<BTreeSet<String>> {
        let mut names = BTreeSet::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let Some(name) = file_name.strip_suffix(".applied.yml") else {
                continue;
            };
            if !is_safe_service_name(name) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "unsafe applied-state snapshot name",
                ));
            }
            names.insert(name.to_owned());
            if names.len() > DEFAULT_MAX_DEFINITIONS {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "applied-state snapshot count exceeds definition limit",
                ));
            }
        }
        Ok(names)
    }

    fn read_owned_file(&self, path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        if !self.state_file_is_safe(&metadata) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe reconciliation state file",
            ));
        }
        if metadata.len() > limit as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state file exceeds size limit",
            ));
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = options.open(path)?;
        let before = file.metadata()?;
        if !self.state_file_is_safe(&before) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "unsafe opened reconciliation state file",
            ));
        }
        if metadata_changed(&metadata, &before) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state changed before open",
            ));
        }
        if before.len() > limit as u64 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state file exceeds size limit",
            ));
        }
        let capacity = usize::try_from(before.len()).map_or(limit, |size| size.min(limit));
        let mut bytes = Vec::with_capacity(capacity);
        (&mut file)
            .take(limit.saturating_add(1) as u64)
            .read_to_end(&mut bytes)?;
        let file_after = file.metadata()?;
        let path_after = fs::symlink_metadata(path)?;
        if bytes.len() > limit
            || !self.state_file_is_safe(&file_after)
            || !self.state_file_is_safe(&path_after)
            || metadata_changed(&before, &file_after)
            || metadata_changed(&before, &path_after)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reconciliation state changed during read",
            ));
        }
        Ok(Some(bytes))
    }

    fn state_file_is_safe(&self, metadata: &Metadata) -> bool {
        metadata.is_file() && metadata.uid() == self.owner_uid && metadata.mode() & 0o777 == 0o600
    }

    fn publish_named(&self, name: &str, kind: &str, config: &ServiceConfig) -> io::Result<PathBuf> {
        let contents = emit_config(config).map_err(io::Error::other)?;
        let destination = self.named_path(name, kind)?;
        self.atomic_replace(&destination, name, contents.as_bytes())?;
        Ok(destination)
    }

    fn atomic_replace(&self, destination: &Path, label: &str, contents: &[u8]) -> io::Result<()> {
        let sequence = NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed);
        let temporary =
            self.directory
                .join(format!(".{label}.{}.{}.tmp", std::process::id(), sequence));
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o600)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let result = (|| -> io::Result<()> {
            let mut file = options.open(&temporary)?;
            file.write_all(contents)?;
            file.sync_all()?;
            fs::rename(&temporary, destination)?;
            File::open(&self.directory)?.sync_all()
        })();
        if result.is_err() {
            let _ignored = fs::remove_file(&temporary);
        }
        result
    }

    fn named_path(&self, name: &str, kind: &str) -> io::Result<PathBuf> {
        if !is_safe_service_name(name) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsafe snapshot service name",
            ));
        }
        Ok(self.directory.join(format!("{name}.{kind}.yml")))
    }
}
