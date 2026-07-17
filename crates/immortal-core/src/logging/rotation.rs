//! Rotating file sink and Immortal-owned archive retention.
//!
//! This child owns sync-before-rename rotation, archive-name allocation,
//! retention pruning, and archive discovery. Retention only removes regular
//! non-symlink siblings matching the exact Immortal archive namespace, and
//! write paths report I/O and policy failures without silently losing bytes.

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use super::LoggingError;
use crate::config::FileLogConfig;

/// Rotation and retention policy used by the external file adapter.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RotationPolicy {
    /// Rotate before a write would exceed this many bytes.
    pub max_bytes: Option<u64>,
    /// Rotate a nonempty file after this elapsed age.
    pub max_age: Option<Duration>,
    /// Maximum number of Immortal-owned archives.
    pub keep: Option<usize>,
    /// Maximum combined bytes across Immortal-owned archives.
    pub max_total_bytes: Option<u64>,
}

impl RotationPolicy {
    /// Convert validated service configuration into adapter limits.
    ///
    /// # Errors
    ///
    /// Returns an error if the archive count cannot fit the target platform's
    /// address space.
    pub fn from_config(config: &FileLogConfig) -> Result<Self, LoggingError> {
        Ok(Self {
            max_bytes: config.max_bytes,
            max_age: config.max_age_seconds.map(Duration::from_secs),
            keep: config
                .keep
                .map(usize::try_from)
                .transpose()
                .map_err(|_| LoggingError::RetentionCountTooLarge)?,
            max_total_bytes: None,
        })
    }
}

/// One Immortal-owned rotated file and its parsed storage identity.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Archive {
    path: PathBuf,
    unix_nanoseconds: u128,
    adapter_pid: u32,
    sequence: u64,
    byte_length: u64,
}

impl Archive {
    /// Archive path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Rotation time measured in nanoseconds since the Unix epoch.
    #[must_use]
    pub const fn unix_nanoseconds(&self) -> u128 {
        self.unix_nanoseconds
    }

    /// Process identifier of the `immortallog` adapter that rotated the file.
    #[must_use]
    pub const fn adapter_pid(&self) -> u32 {
        self.adapter_pid
    }

    /// Per-adapter sequence used to disambiguate archive names.
    #[must_use]
    pub const fn sequence(&self) -> u64 {
        self.sequence
    }

    /// File length observed while scanning the archive directory.
    #[must_use]
    pub const fn byte_length(&self) -> u64 {
        self.byte_length
    }
}

/// List archives owned by one live-file namespace in chronological order.
///
/// The live file need not exist. Only regular non-symlink siblings named
/// `<live>.@<unix-nanoseconds>.<adapter-pid>.<sequence>` are returned.
///
/// # Errors
///
/// Returns an error when the live filename is not UTF-8 or its parent
/// directory or matching archive metadata cannot be read.
pub fn archives(path: &Path) -> io::Result<Vec<Archive>> {
    let (parent, prefix) = archive_location(path)?;
    let mut archives = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        let archive_path = entry.path();
        let Some(name) = archive_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some((unix_nanoseconds, adapter_pid, sequence)) = archive_identity(name, &prefix)
        else {
            continue;
        };
        let metadata = fs::symlink_metadata(&archive_path)?;
        if metadata.is_file() && !metadata.file_type().is_symlink() {
            archives.push(Archive {
                path: archive_path,
                unix_nanoseconds,
                adapter_pid,
                sequence,
                byte_length: metadata.len(),
            });
        }
    }
    archives.sort_by_key(|archive| {
        (
            archive.unix_nanoseconds,
            archive.adapter_pid,
            archive.sequence,
        )
    });
    Ok(archives)
}

/// Append-only file sink with atomic rename rotation and bounded retention.
///
/// Rotation syncs the live file before renaming it into Immortal's `.@`
/// namespace. Retention never removes siblings outside the exact archive shape.
pub struct RotatingFile {
    path: PathBuf,
    file: File,
    policy: RotationPolicy,
    bytes_written: u64,
    opened_at: SystemTime,
    archive_sequence: u64,
}

impl RotatingFile {
    /// Open or create a destination and enforce existing archive retention.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid destination, open/metadata failure, or
    /// inability to enforce retention.
    pub fn open(path: &Path, policy: RotationPolicy) -> io::Result<Self> {
        if path.file_name().is_none() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log destination must have a filename",
            ));
        }
        validate_rotation_policy(policy)?;
        let file = open_append(path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log destination is not a regular file",
            ));
        }
        let mut sink = Self {
            path: path.to_owned(),
            file,
            policy,
            bytes_written: metadata.len(),
            opened_at: metadata.modified().unwrap_or_else(|_| SystemTime::now()),
            archive_sequence: 0,
        };
        sink.enforce_retention()?;
        Ok(sink)
    }

    /// Write an entire byte slice, rotating once before it when required.
    ///
    /// # Errors
    ///
    /// Returns an error for rotation, retention, or write failure. Partial
    /// writes are never reported as success.
    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.write_parts(&[bytes])
    }

    /// Write multiple parts as one rotation unit.
    ///
    /// This keeps a timestamp prefix and its record in the same archive without
    /// buffering arbitrarily large lines.
    ///
    /// # Errors
    ///
    /// Returns an error for length overflow, rotation, retention, or write failure.
    pub fn write_parts(&mut self, parts: &[&[u8]]) -> io::Result<()> {
        let incoming = parts.iter().try_fold(0_u64, |total, part| {
            u64::try_from(part.len())
                .ok()
                .and_then(|length| total.checked_add(length))
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "log write is too large")
                })
        })?;
        self.rotate_if_needed(incoming)?;
        for part in parts {
            self.file.write_all(part)?;
        }
        self.bytes_written = self.bytes_written.saturating_add(incoming);
        Ok(())
    }

    /// Flush userspace buffers and request durable file contents.
    ///
    /// # Errors
    ///
    /// Returns an error from flush or `sync_all`.
    pub fn sync(&mut self) -> io::Result<()> {
        self.file.flush()?;
        self.file.sync_all()
    }

    /// Current destination path.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn rotate_if_needed(&mut self, incoming: u64) -> io::Result<()> {
        if self.bytes_written == 0 {
            return Ok(());
        }
        let size_due = self
            .policy
            .max_bytes
            .is_some_and(|limit| self.bytes_written.saturating_add(incoming) > limit);
        let age_due = self.policy.max_age.is_some_and(|limit| {
            SystemTime::now()
                .duration_since(self.opened_at)
                .is_ok_and(|age| age >= limit)
        });
        if size_due || age_due {
            self.rotate()?;
        }
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.sync()?;
        let archive = self.next_archive_path()?;
        fs::rename(&self.path, &archive)?;
        let replacement = match open_append(&self.path) {
            Ok(file) => file,
            Err(error) => {
                let _rollback = fs::rename(&archive, &self.path);
                return Err(error);
            }
        };
        self.file = replacement;
        self.bytes_written = 0;
        self.opened_at = SystemTime::now();
        sync_parent(&self.path)?;
        self.enforce_retention()
    }

    fn next_archive_path(&mut self) -> io::Result<PathBuf> {
        let (parent, prefix) = archive_location(&self.path)?;
        let timestamp = unix_nanoseconds(SystemTime::now())?;
        for _ in 0..1024 {
            self.archive_sequence = self
                .archive_sequence
                .checked_add(1)
                .ok_or_else(|| io::Error::other("log archive sequence is exhausted"))?;
            let candidate = parent.join(format!(
                "{prefix}{timestamp}.{}.{}",
                std::process::id(),
                self.archive_sequence
            ));
            if archive_path_is_available(&candidate)? {
                return Ok(candidate);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "unable to allocate unique log archive name",
        ))
    }

    fn enforce_retention(&mut self) -> io::Result<()> {
        let archives = archives(&self.path)?;
        let mut total_bytes = archives.iter().fold(0_u64, |total, archive| {
            total.saturating_add(archive.byte_length)
        });
        let mut remaining = archives.len();
        for archive in archives {
            let over_count = self.policy.keep.is_some_and(|keep| remaining > keep);
            let over_bytes = self
                .policy
                .max_total_bytes
                .is_some_and(|limit| total_bytes > limit);
            if !(over_count || over_bytes) {
                break;
            }
            fs::remove_file(archive.path)?;
            remaining = remaining.saturating_sub(1);
            total_bytes = total_bytes.saturating_sub(archive.byte_length);
        }
        Ok(())
    }
}

fn archive_location(path: &Path) -> io::Result<(&Path, String)> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let filename = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "log filename is not UTF-8"))?;
    Ok((parent, format!("{filename}.@")))
}

fn archive_identity(name: &str, prefix: &str) -> Option<(u128, u32, u64)> {
    let mut fields = name.strip_prefix(prefix)?.split('.');
    let timestamp = fields.next()?;
    let process = fields.next()?;
    let sequence = fields.next()?;
    if fields.next().is_some()
        || [timestamp, process, sequence]
            .iter()
            .any(|field| field.is_empty() || !field.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return None;
    }
    Some((
        timestamp.parse().ok()?,
        process.parse().ok()?,
        sequence.parse().ok()?,
    ))
}

fn unix_nanoseconds(time: SystemTime) -> io::Result<u128> {
    time.duration_since(UNIX_EPOCH)
        .map_err(|_| io::Error::other("system clock is before the Unix epoch"))
        .map(|elapsed| elapsed.as_nanos())
}

fn archive_path_is_available(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error),
    }
}

fn validate_rotation_policy(policy: RotationPolicy) -> io::Result<()> {
    if policy.max_bytes == Some(0)
        || policy.max_age == Some(Duration::ZERO)
        || policy.keep == Some(0)
        || policy.max_total_bytes == Some(0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "configured rotation limits must be greater than zero",
        ));
    }
    Ok(())
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    File::open(parent)?.sync_all()
}

#[cfg(test)]
mod tests;
