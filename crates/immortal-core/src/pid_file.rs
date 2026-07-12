//! Atomic, output-only PID publication with replacement-safe cleanup.

use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// One atomically published PID file removed only while it remains ours.
#[derive(Debug)]
pub(crate) struct OwnedPidFile {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl OwnedPidFile {
    pub(crate) fn publish(path: &Path, process: u32) -> io::Result<Self> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "PID file has no parent directory",
            )
        })?;
        let file_name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "PID file has no filename")
        })?;
        let temporary = temporary_path(parent, file_name);
        let mut cleanup = TemporaryFile::new(temporary.clone());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .mode(0o644)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
        let mut file = options.open(&temporary)?;
        file.set_permissions(fs::Permissions::from_mode(0o644))?;
        writeln!(file, "{process}")?;
        file.sync_all()?;
        let metadata = file.metadata()?;
        fs::rename(&temporary, path)?;
        cleanup.disarm();
        sync_directory(parent)?;
        Ok(Self {
            path: path.to_owned(),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    fn still_owned(&self) -> bool {
        fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.dev() == self.device
                && metadata.ino() == self.inode
        })
    }
}

impl Drop for OwnedPidFile {
    fn drop(&mut self) {
        if !self.still_owned() {
            return;
        }
        if fs::remove_file(&self.path).is_ok()
            && let Some(parent) = self.path.parent()
        {
            let _ = sync_directory(parent);
        }
    }
}

fn temporary_path(parent: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
    let mut name = OsString::from(".");
    name.push(file_name);
    name.push(format!(".immortal-{}-{sequence}.tmp", std::process::id()));
    parent.join(name)
}

fn sync_directory(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

struct TemporaryFile {
    path: PathBuf,
    armed: bool,
}

impl TemporaryFile {
    const fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, fs, path::PathBuf, sync::atomic::AtomicU64};

    use super::OwnedPidFile;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn publication_is_atomic_and_cleanup_removes_the_owned_inode() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("service.pid");
        let owner = OwnedPidFile::publish(&path, 42)?;
        assert_eq!(fs::read_to_string(&path)?, "42\n");
        drop(owner);
        assert!(!path.exists());
        Ok(())
    }

    #[test]
    fn cleanup_never_removes_a_replacement() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("service.pid");
        let displaced = directory.path().join("displaced.pid");
        let owner = OwnedPidFile::publish(&path, 42)?;
        fs::rename(&path, displaced)?;
        fs::write(&path, b"replacement\n")?;
        drop(owner);
        assert_eq!(fs::read_to_string(path)?, "replacement\n");
        Ok(())
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> std::io::Result<Self> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortal-pid-file-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            Ok(Self(path))
        }

        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TestDirectory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
