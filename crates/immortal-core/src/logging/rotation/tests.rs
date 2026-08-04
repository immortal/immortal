//! Rotation archive and sink behavior tests.

use std::{
    error::Error,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread,
    time::{Duration, UNIX_EPOCH},
};

use super::{RotatingFile, RotationPolicy, archive_path_is_available, archives, unix_nanoseconds};

/// Bounded attempts at scheduling a retention race, so the test always ends.
const RACE_ITERATIONS: u32 = 3_000;
/// Archive-shaped files created per churn round by the racing thread.
const ARCHIVE_CHURN: u32 = 16;

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

/// Regression: an age-only policy never rotated across adapter restarts.
///
/// `opened_at` was seeded from the modification time, so reopening a log that
/// had just been appended to restarted the age clock. With no size limit
/// configured nothing else could trigger a rotation, and the file grew without
/// bound. The clock must follow when the live file was created, not when it was
/// last touched.
///
/// The sleep separates creation time from modification time, which is the only
/// way the two seeds differ; it is bounded well above the age limit so the
/// modification-time seeding cannot pass by accident.
#[test]
fn reopening_does_not_reset_the_age_clock() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let policy = RotationPolicy {
        max_age: Some(Duration::from_millis(150)),
        ..RotationPolicy::default()
    };
    let mut sink = RotatingFile::open(&path, policy)?;
    sink.write_all(b"first\n")?;
    drop(sink);

    thread::sleep(Duration::from_millis(400));
    fs::OpenOptions::new()
        .append(true)
        .open(&path)?
        .write_all(b"extra\n")?;

    // The live file is now far older than the limit even though it was
    // modified moments ago.
    let mut sink = RotatingFile::open(&path, policy)?;
    sink.write_all(b"second\n")?;
    drop(sink);

    let rotated = archives(&path)?;
    let [archive] = rotated.as_slice() else {
        return Err("an aged live file must rotate after a restart".into());
    };
    assert_eq!(fs::read(&archive.path)?, b"first\nextra\n");
    assert_eq!(fs::read(&path)?, b"second\n");
    Ok(())
}

/// A fresh destination is not immediately due for age-based rotation.
#[test]
fn a_new_destination_is_not_immediately_aged_out() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let mut sink = RotatingFile::open(
        &path,
        RotationPolicy {
            max_age: Some(Duration::from_hours(1)),
            ..RotationPolicy::default()
        },
    )?;
    sink.write_all(b"first\n")?;
    sink.write_all(b"second\n")?;
    drop(sink);

    assert!(archives(&path)?.is_empty());
    assert_eq!(fs::read(&path)?, b"first\nsecond\n");
    Ok(())
}

/// Regression: an archive removed concurrently failed the log write.
///
/// Retention listed the archive directory, called `symlink_metadata` on every
/// candidate, then unlinked the oldest ones. An operator or a peer adapter
/// cleaning the same directory could remove an archive between any two of those
/// steps, and the resulting `NotFound` propagated out of `write_all` as a hard
/// logging failure. Losing that race means the goal was already met, so it must
/// be absorbed.
///
/// The race is scheduled rather than simulated: a remover thread churns the
/// same archives while this thread keeps listing and rotating. The loop is
/// bounded so the test terminates whether or not the race is observed.
#[test]
fn a_concurrent_remover_never_fails_retention() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let policy = RotationPolicy {
        max_bytes: Some(4),
        keep: Some(2),
        ..RotationPolicy::default()
    };
    let mut sink = RotatingFile::open(&path, policy)?;
    let stop = Arc::new(AtomicBool::new(false));
    let remover = {
        let stop = Arc::clone(&stop);
        let path = path.clone();
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Ok(existing) = archives(&path) else {
                    continue;
                };
                for archive in existing {
                    let _ignored = fs::remove_file(&archive.path);
                }
            }
        })
    };

    let outcome = (0..RACE_ITERATIONS).try_for_each(|_| sink.write_all(b"aaaaa"));
    stop.store(true, Ordering::Relaxed);
    remover.join().map_err(|_| "remover thread panicked")?;
    outcome?;
    Ok(())
}

/// Listing archives tolerates entries removed during the directory walk.
#[test]
fn archives_tolerates_a_concurrent_remover() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    let path = directory.path().join("api.log");
    let stop = Arc::new(AtomicBool::new(false));
    let churn = {
        let stop = Arc::clone(&stop);
        let directory = directory.path().to_owned();
        thread::spawn(move || {
            let mut sequence = 0_u64;
            while !stop.load(Ordering::Relaxed) {
                let mut created = Vec::new();
                for index in 0..ARCHIVE_CHURN {
                    sequence = sequence.wrapping_add(1);
                    let candidate = directory.join(format!(
                        "api.log.@{sequence}.{}.{index}",
                        std::process::id()
                    ));
                    if fs::write(&candidate, b"x").is_ok() {
                        created.push(candidate);
                    }
                }
                for candidate in created {
                    let _ignored = fs::remove_file(candidate);
                }
            }
        })
    };

    let outcome = (0..RACE_ITERATIONS).try_for_each(|_| archives(&path).map(|_| ()));
    stop.store(true, Ordering::Relaxed);
    churn.join().map_err(|_| "churn thread panicked")?;
    outcome?;
    Ok(())
}
