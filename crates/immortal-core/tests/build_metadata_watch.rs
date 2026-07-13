use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[path = "../build_support.rs"]
mod build_support;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> io::Result<Self> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "immortal-build-metadata-{}-{sequence}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir(&path)?;
        Ok(Self(fs::canonicalize(path)?))
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
fn normal_repository_watches_head_ref_and_packed_refs() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let git = directory.path().join(".git");
    let reference = git.join("refs/heads/rust");
    fs::create_dir_all(
        reference.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "reference has no parent")
        })?,
    )?;
    fs::write(git.join("HEAD"), "ref: refs/heads/rust\n")?;
    fs::write(&reference, "0123456789abcdef\n")?;
    fs::write(git.join("packed-refs"), "# pack-refs\n")?;

    let paths = build_support::git_watch_paths(directory.path());
    assert!(paths.contains(&git.join("HEAD")));
    assert!(paths.contains(&reference));
    assert!(paths.contains(&git.join("packed-refs")));
    Ok(())
}

#[test]
fn linked_worktree_resolves_common_refs() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let workspace = directory.path().join("worktree");
    let common = directory.path().join("repository/.git");
    let git = common.join("worktrees/rust");
    let reference = common.join("refs/heads/rust");
    fs::create_dir_all(&workspace)?;
    fs::create_dir_all(&git)?;
    fs::create_dir_all(
        reference.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "reference has no parent")
        })?,
    )?;
    fs::write(
        workspace.join(".git"),
        format!("gitdir: {}\n", git.display()),
    )?;
    fs::write(git.join("commondir"), "../..\n")?;
    fs::write(git.join("HEAD"), "ref: refs/heads/rust\n")?;
    fs::write(&reference, "0123456789abcdef\n")?;

    let paths = build_support::git_watch_paths(&workspace);
    assert!(paths.contains(&workspace.join(".git")));
    assert!(paths.contains(&git.join("commondir")));
    assert!(paths.contains(&git.join("HEAD")));
    assert!(paths.contains(&reference));
    Ok(())
}

#[test]
fn packaged_source_without_git_metadata_has_no_watch_paths() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    assert!(build_support::git_watch_paths(directory.path()).is_empty());
    Ok(())
}

#[test]
fn malformed_and_oversized_git_pointers_are_bounded() -> io::Result<()> {
    let directory = TestDirectory::new()?;
    let marker = directory.path().join(".git");
    fs::write(&marker, "not-a-git-pointer\nsecond-line\n")?;
    let malformed_paths = build_support::git_watch_paths(directory.path());
    assert_eq!(malformed_paths.first(), Some(&marker));
    assert_eq!(malformed_paths.len(), 1);

    fs::write(&marker, vec![b'x'; 4_097])?;
    let oversized_paths = build_support::git_watch_paths(directory.path());
    assert_eq!(oversized_paths.first(), Some(&marker));
    assert_eq!(oversized_paths.len(), 1);
    Ok(())
}
