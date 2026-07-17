//! Go-compatible direct-command environment-directory loader.
//!
//! [`load_environment_directory`] materializes a bounded, detached
//! `BTreeMap` from one directory of small files so daemon startup and every
//! later service generation observe the same values regardless of later
//! filesystem changes. Every read is guarded against time-of-check to
//! time-of-use races: the directory and each regular file are re-inspected
//! by device and inode identity, size, and modification time after every
//! operation that could race with a concurrent replacement, and symlinks are
//! never followed. [`environment_key_is_valid`] and
//! [`environment_value_is_valid`] define the same key/value shape enforced
//! both here and by [`super::validate`] for configuration-file environment
//! maps, so both loading paths share one notion of a well-formed entry.

use std::{
    collections::BTreeMap,
    fs::{self, Metadata, OpenOptions},
    io::{self, BufRead, BufReader, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

use super::error::ConfigError;

/// Maximum number of entries inspected in one direct-command environment directory.
pub const MAX_ENVIRONMENT_DIRECTORY_ENTRIES: usize = 4_096;
/// Maximum accepted byte length of one environment-file first line.
pub const MAX_ENVIRONMENT_VALUE_BYTES: usize = 256 * 1024;
/// Maximum aggregate byte length of loaded environment keys and values.
pub const MAX_ENVIRONMENT_DIRECTORY_BYTES: usize = super::MAX_CONFIG_BYTES;

const MAX_ENVIRONMENT_VALUE_READ_BYTES: u64 = 256 * 1024 + 2;

/// Load the Go-compatible direct-command environment-directory format.
///
/// Each regular file contributes its UTF-8 filename and first UTF-8 line. A
/// physically empty file contributes nothing, while a first empty line sets an
/// empty value. CRLF is normalized like Go's line scanner. Symlinks and other
/// non-regular entries are never followed and do not contribute values.
///
/// The returned snapshot is bounded and detached from the directory, so daemon
/// startup and every later service generation use the same materialized values.
///
/// # Errors
///
/// Returns an error when the directory is absent, symlinked, not a directory,
/// changes during the scan, contains an unreadable or changing regular file, or
/// exceeds the entry, first-line, aggregate-size, UTF-8, or environment bounds.
pub fn load_environment_directory(
    directory: &Path,
) -> Result<BTreeMap<String, String>, ConfigError> {
    let directory_before =
        fs::symlink_metadata(directory).map_err(|error| environment_error(directory, error))?;
    if directory_before.file_type().is_symlink() || !directory_before.is_dir() {
        return Err(invalid_environment_input(
            directory,
            "environment path must be a real directory, not a symlink",
        ));
    }
    let canonical =
        fs::canonicalize(directory).map_err(|error| environment_error(directory, error))?;
    let canonical_before =
        fs::metadata(&canonical).map_err(|error| environment_error(&canonical, error))?;
    if environment_directory_changed(&directory_before, &canonical_before) {
        return Err(invalid_environment_input(
            directory,
            "environment directory changed during validation",
        ));
    }

    let entries = fs::read_dir(&canonical).map_err(|error| environment_error(&canonical, error))?;
    let mut environment = BTreeMap::new();
    let mut entry_count = 0_usize;
    let mut total_bytes = 0_usize;
    for entry in entries {
        entry_count = entry_count.checked_add(1).ok_or_else(|| {
            invalid_environment_input(&canonical, "environment entry count overflowed")
        })?;
        if entry_count > MAX_ENVIRONMENT_DIRECTORY_ENTRIES {
            return Err(invalid_environment_input(
                &canonical,
                format!(
                    "environment directory has more than {MAX_ENVIRONMENT_DIRECTORY_ENTRIES} entries"
                ),
            ));
        }
        let entry = entry.map_err(|error| environment_error(&canonical, error))?;
        let path = entry.path();
        let metadata =
            fs::symlink_metadata(&path).map_err(|error| environment_error(&path, error))?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            continue;
        }
        let key = entry.file_name().into_string().map_err(|_| {
            invalid_environment_input(&path, "environment filename is not valid UTF-8")
        })?;
        if !environment_key_is_valid(&key) {
            return Err(invalid_environment_input(
                &path,
                format!("environment filename {key:?} is not a valid key"),
            ));
        }
        let Some(value) = read_environment_value(&path, &metadata)? else {
            continue;
        };
        let entry_bytes = key
            .len()
            .checked_add(value.len())
            .ok_or_else(|| invalid_environment_input(&path, "environment entry size overflowed"))?;
        total_bytes = total_bytes.checked_add(entry_bytes).ok_or_else(|| {
            invalid_environment_input(&canonical, "environment aggregate size overflowed")
        })?;
        if total_bytes > MAX_ENVIRONMENT_DIRECTORY_BYTES {
            return Err(invalid_environment_input(
                &canonical,
                format!(
                    "environment keys and values exceed {MAX_ENVIRONMENT_DIRECTORY_BYTES} bytes"
                ),
            ));
        }
        environment.insert(key, value);
    }

    let directory_after =
        fs::metadata(&canonical).map_err(|error| environment_error(&canonical, error))?;
    if environment_directory_changed(&canonical_before, &directory_after) {
        return Err(invalid_environment_input(
            &canonical,
            "environment directory changed during the scan",
        ));
    }
    Ok(environment)
}

fn read_environment_value(
    path: &Path,
    path_before: &Metadata,
) -> Result<Option<String>, ConfigError> {
    let mut options = OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let mut file = options
        .open(path)
        .map_err(|error| environment_error(path, error))?;
    let file_before = file
        .metadata()
        .map_err(|error| environment_error(path, error))?;
    if environment_file_changed(path_before, &file_before) {
        return Err(invalid_environment_input(
            path,
            "environment file changed before it was opened",
        ));
    }

    let mut bytes = Vec::new();
    {
        let mut reader = BufReader::new((&mut file).take(MAX_ENVIRONMENT_VALUE_READ_BYTES));
        reader
            .read_until(b'\n', &mut bytes)
            .map_err(|error| environment_error(path, error))?;
    }

    let file_after = file
        .metadata()
        .map_err(|error| environment_error(path, error))?;
    let path_after = fs::symlink_metadata(path).map_err(|error| environment_error(path, error))?;
    if path_after.file_type().is_symlink()
        || environment_file_changed(&file_before, &file_after)
        || environment_file_changed(&file_before, &path_after)
    {
        return Err(invalid_environment_input(
            path,
            "environment file changed while it was read",
        ));
    }

    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    if bytes.len() > MAX_ENVIRONMENT_VALUE_BYTES {
        return Err(invalid_environment_input(
            path,
            format!(
                "environment first line is {} bytes; limit is {MAX_ENVIRONMENT_VALUE_BYTES}",
                bytes.len()
            ),
        ));
    }
    if bytes.contains(&0) {
        return Err(invalid_environment_input(
            path,
            "environment first line contains NUL",
        ));
    }
    String::from_utf8(bytes).map(Some).map_err(|error| {
        invalid_environment_input(
            path,
            format!("environment first line is not valid UTF-8: {error}"),
        )
    })
}

fn environment_error(path: &Path, source: io::Error) -> ConfigError {
    ConfigError::EnvironmentInput {
        path: path.to_owned(),
        source,
    }
}

fn invalid_environment_input(path: &Path, message: impl Into<String>) -> ConfigError {
    environment_error(
        path,
        io::Error::new(io::ErrorKind::InvalidData, message.into()),
    )
}

fn environment_directory_changed(before: &Metadata, after: &Metadata) -> bool {
    !same_file_identity(before, after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || !after.is_dir()
}

fn environment_file_changed(before: &Metadata, after: &Metadata) -> bool {
    !same_file_identity(before, after)
        || before.len() != after.len()
        || before.modified().ok() != after.modified().ok()
        || !after.is_file()
}

fn same_file_identity(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

/// Whether `key` is an acceptable environment-variable name.
pub(super) fn environment_key_is_valid(key: &str) -> bool {
    !key.is_empty() && !key.contains('=') && !key.contains('\0')
}

/// Whether `value` is an acceptable environment-variable value.
pub(super) fn environment_value_is_valid(value: &str) -> bool {
    !value.contains('\0')
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        error::Error,
        fs, io,
        os::unix::fs::symlink,
        path::{Path, PathBuf},
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{
        MAX_ENVIRONMENT_DIRECTORY_BYTES, MAX_ENVIRONMENT_DIRECTORY_ENTRIES,
        MAX_ENVIRONMENT_VALUE_BYTES, load_environment_directory,
    };

    #[test]
    fn environment_directory_loads_regular_file_first_lines() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new("environment-load")?;
        fs::write(directory.join("DEBUG"), "true\nignored\n")?;
        fs::write(directory.join("ENVIRONMENT"), "production\r\nignored\n")?;
        fs::write(directory.join("EMPTY"), "\nignored\n")?;
        fs::write(directory.join("ABSENT"), "")?;
        fs::create_dir(directory.join("nested"))?;
        symlink(directory.join("DEBUG"), directory.join("LINK"))?;

        let environment = load_environment_directory(directory.path())?;
        assert_eq!(
            environment,
            BTreeMap::from([
                ("DEBUG".to_owned(), "true".to_owned()),
                ("EMPTY".to_owned(), String::new()),
                ("ENVIRONMENT".to_owned(), "production".to_owned()),
            ])
        );
        Ok(())
    }

    #[test]
    fn environment_directory_rejects_unsafe_or_malformed_inputs() -> Result<(), Box<dyn Error>> {
        let parent = TestDirectory::new("environment-invalid")?;
        let directory = parent.join("actual");
        fs::create_dir(&directory)?;
        let link = parent.join("link");
        symlink(&directory, &link)?;
        assert!(load_environment_directory(&link).is_err());
        assert!(load_environment_directory(&parent.join("missing")).is_err());

        let invalid_key = directory.join("BAD=KEY");
        fs::write(&invalid_key, "value\n")?;
        assert!(load_environment_directory(&directory).is_err());
        fs::remove_file(&invalid_key)?;

        let invalid_value = directory.join("INVALID_UTF8");
        fs::write(&invalid_value, [0xff, b'\n'])?;
        assert!(load_environment_directory(&directory).is_err());
        fs::remove_file(&invalid_value)?;

        let oversized = directory.join("OVERSIZED");
        fs::write(&oversized, vec![b'x'; MAX_ENVIRONMENT_VALUE_BYTES + 1])?;
        assert!(load_environment_directory(&directory).is_err());
        Ok(())
    }

    #[test]
    fn environment_directory_enforces_entry_and_aggregate_bounds() -> Result<(), Box<dyn Error>> {
        let entries = TestDirectory::new("environment-entry-bound")?;
        for index in 0..=MAX_ENVIRONMENT_DIRECTORY_ENTRIES {
            fs::write(entries.join(format!("ENTRY_{index}")), "")?;
        }
        assert!(load_environment_directory(entries.path()).is_err());

        let aggregate = TestDirectory::new("environment-aggregate-bound")?;
        let value = vec![b'x'; MAX_ENVIRONMENT_VALUE_BYTES];
        let files = MAX_ENVIRONMENT_DIRECTORY_BYTES
            .checked_div(MAX_ENVIRONMENT_VALUE_BYTES)
            .and_then(|count| count.checked_add(1))
            .ok_or("environment test bound overflowed")?;
        for index in 0..files {
            fs::write(aggregate.join(format!("VALUE_{index}")), &value)?;
        }
        assert!(load_environment_directory(aggregate.path()).is_err());
        Ok(())
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new(name: &str) -> io::Result<Self> {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_err(io::Error::other)?
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "immortal-config-{name}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &Path {
            &self.0
        }

        fn join(&self, path: impl AsRef<Path>) -> PathBuf {
            self.0.join(path)
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
