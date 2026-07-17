//! Rotation archive and sink behavior tests.

use std::{
    error::Error,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::{Duration, UNIX_EPOCH},
};

use super::{RotatingFile, RotationPolicy, archive_path_is_available, archives, unix_nanoseconds};

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);
struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "immortal-logging-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        Ok(Self(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ignored = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn rotation_syncs_and_enforces_archive_count() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let mut sink = RotatingFile::open(
        &path,
        RotationPolicy {
            max_bytes: Some(5),
            keep: Some(2),
            ..RotationPolicy::default()
        },
    )?;
    for bytes in [b"aaa".as_slice(), b"bbb", b"ccc", b"ddd"] {
        sink.write_all(bytes)?;
    }
    sink.sync()?;

    assert_eq!(fs::read(&path)?, b"ddd");
    let archives = archive_contents(directory.path(), "api.log")?;
    assert_eq!(archives, [b"bbb".to_vec(), b"ccc".to_vec()]);
    Ok(())
}

#[test]
fn rotation_enforces_total_archive_bytes() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let mut sink = RotatingFile::open(
        &path,
        RotationPolicy {
            max_bytes: Some(3),
            keep: Some(10),
            max_total_bytes: Some(3),
            ..RotationPolicy::default()
        },
    )?;
    sink.write_all(b"aaa")?;
    sink.write_all(b"bbb")?;
    sink.write_all(b"ccc")?;

    let archives = archive_contents(directory.path(), "api.log")?;
    assert_eq!(archives, [b"bbb".to_vec()]);
    Ok(())
}

#[test]
fn archive_timestamp_rejects_pre_epoch_clock() -> Result<(), Box<dyn Error>> {
    let before_epoch = UNIX_EPOCH
        .checked_sub(Duration::from_nanos(1))
        .ok_or("could not represent a pre-epoch test time")?;
    let error = unix_nanoseconds(before_epoch)
        .err()
        .ok_or("pre-epoch time was accepted")?;
    assert!(error.to_string().contains("before the Unix epoch"));
    Ok(())
}

#[test]
fn archive_sequence_exhaustion_is_reported() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let mut sink = RotatingFile::open(&path, RotationPolicy::default())?;
    sink.archive_sequence = u64::MAX;

    let error = sink
        .next_archive_path()
        .err()
        .ok_or("exhausted archive sequence was accepted")?;
    assert!(error.to_string().contains("sequence is exhausted"));
    Ok(())
}

#[cfg(unix)]
#[test]
fn archive_allocation_treats_dangling_symlink_as_a_collision() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let candidate = directory.path().join("api.log.@1.2.3");
    std::os::unix::fs::symlink(directory.path().join("missing"), &candidate)?;

    assert!(!archive_path_is_available(&candidate)?);
    assert!(archive_path_is_available(
        &directory.path().join("api.log.@1.2.4")
    )?);
    Ok(())
}

#[test]
fn archive_catalog_uses_exact_ownership_and_numeric_order() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let first = directory.path().join("api.log.@2.3.20");
    let second = directory.path().join("api.log.@2.10.10");
    let third = directory.path().join("api.log.@10.9.2");
    for (archive, contents) in [
        (&first, b"a".as_slice()),
        (&second, b"bb".as_slice()),
        (&third, b"ccc".as_slice()),
    ] {
        fs::write(archive, contents)?;
    }
    for unrelated in [
        "api.log.immortal-archive.1.2.3",
        "api.log.@3.4",
        "api.log.@3.4.5.extra",
        "api.log.@not-a-time.4.5",
        "api.log.@+3.4.5",
        "api.log.@3.+4.5",
        "api.log.@3.4.+5",
        "api.log.@340282366920938463463374607431768211456.4.5",
    ] {
        fs::write(directory.path().join(unrelated), b"unowned")?;
    }
    fs::create_dir(directory.path().join("api.log.@3.4.5"))?;
    #[cfg(unix)]
    std::os::unix::fs::symlink(&first, directory.path().join("api.log.@4.5.6"))?;

    let found = archives(&path)?;
    let paths: Vec<&Path> = found.iter().map(super::Archive::path).collect();
    assert_eq!(paths, [&first, &second, &third]);
    assert_eq!(found.first().map(super::Archive::unix_nanoseconds), Some(2));
    assert_eq!(found.first().map(super::Archive::adapter_pid), Some(3));
    assert_eq!(found.first().map(super::Archive::sequence), Some(20));
    assert_eq!(found.first().map(super::Archive::byte_length), Some(1));
    Ok(())
}

#[test]
fn reopen_recovers_rotation_state_without_removing_unowned_files() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let older = directory.path().join("api.log.@100.7.1");
    let newer = directory.path().join("api.log.@200.7.2");
    let old_prototype = directory.path().join("api.log.immortal-archive.50.7.1");
    let malformed = directory.path().join("api.log.@operator-copy");
    fs::write(&older, b"old")?;
    fs::write(&newer, b"new")?;
    fs::write(&old_prototype, b"prototype")?;
    fs::write(&malformed, b"operator")?;

    let mut sink = RotatingFile::open(
        &path,
        RotationPolicy {
            keep: Some(1),
            ..RotationPolicy::default()
        },
    )?;
    sink.write_all(b"live")?;
    sink.sync()?;

    assert!(!older.exists());
    assert_eq!(fs::read(newer)?, b"new");
    assert_eq!(fs::read(old_prototype)?, b"prototype");
    assert_eq!(fs::read(malformed)?, b"operator");
    assert_eq!(fs::read(path)?, b"live");
    Ok(())
}

#[test]
fn sink_rejects_invalid_limits_and_missing_parent() {
    assert!(
        RotatingFile::open(
            Path::new("/definitely/missing/immortal/api.log"),
            RotationPolicy::default(),
        )
        .is_err()
    );
    assert!(
        RotatingFile::open(
            Path::new("api.log"),
            RotationPolicy {
                max_bytes: Some(0),
                ..RotationPolicy::default()
            },
        )
        .is_err()
    );
}

fn archive_contents(directory: &Path, filename: &str) -> Result<Vec<Vec<u8>>, Box<dyn Error>> {
    archives(&directory.join(filename))?
        .into_iter()
        .map(|archive| fs::read(archive.path).map_err(Into::into))
        .collect()
}
