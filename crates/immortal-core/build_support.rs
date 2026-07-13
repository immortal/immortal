//! Bounded Git metadata discovery for Cargo build-script invalidation.
//!
//! `built` reads the current revision but does not tell Cargo which Git files
//! invalidate that generated value. This module discovers only the active
//! worktree metadata, current symbolic ref, and packed refs. Packaged sources
//! without `.git`, malformed pointers, and concurrent metadata replacement are
//! treated as absent rather than making an otherwise reproducible build fail.

use std::{
    fs,
    path::{Component, Path, PathBuf},
};

const MAX_POINTER_BYTES: u64 = 4_096;

/// Return the bounded set of Git paths which can change the current revision.
pub(crate) fn git_watch_paths(workspace: &Path) -> Vec<PathBuf> {
    let marker = workspace.join(".git");
    let mut paths = Vec::with_capacity(6);
    let git_directory = if marker.is_dir() {
        marker
    } else if marker.is_file() {
        let directory = read_git_directory(&marker);
        push_existing(&mut paths, marker);
        let Some(directory) = directory else {
            return paths;
        };
        directory
    } else {
        return paths;
    };

    let head = git_directory.join("HEAD");
    let symbolic_reference = read_symbolic_reference(&head);
    push_existing(&mut paths, head);

    let common_directory = resolve_common_directory(&git_directory, &mut paths);
    if let Some(reference) = symbolic_reference {
        let reference_path = common_directory.join(reference);
        if reference_path.exists() {
            push_existing(&mut paths, reference_path);
        } else if let Some(parent) = reference_path.parent() {
            push_existing(&mut paths, parent.to_path_buf());
        }
    }
    push_existing(&mut paths, common_directory.join("packed-refs"));
    paths
}

fn resolve_common_directory(git_directory: &Path, paths: &mut Vec<PathBuf>) -> PathBuf {
    let marker = git_directory.join("commondir");
    let common_directory = read_relative_path(&marker, None);
    push_existing(paths, marker);
    common_directory
        .and_then(|path| fs::canonicalize(path).ok())
        .unwrap_or_else(|| git_directory.to_path_buf())
}

fn read_git_directory(marker: &Path) -> Option<PathBuf> {
    read_relative_path(marker, Some("gitdir:"))
        .and_then(|path| fs::canonicalize(path).ok())
        .filter(|path| path.is_dir())
}

fn read_symbolic_reference(head: &Path) -> Option<PathBuf> {
    let head_contents = read_single_line(head)?;
    let reference = head_contents.strip_prefix("ref:")?.trim();
    let path = Path::new(reference);
    if reference.starts_with("refs/")
        && path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        Some(path.to_path_buf())
    } else {
        None
    }
}

fn read_relative_path(marker: &Path, prefix: Option<&str>) -> Option<PathBuf> {
    let line = read_single_line(marker)?;
    let value = match prefix {
        Some(prefix) => line.strip_prefix(prefix)?.trim(),
        None => line.trim(),
    };
    if value.is_empty() {
        return None;
    }
    let path = Path::new(value);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        marker.parent().map(|parent| parent.join(path))
    }
}

fn read_single_line(path: &Path) -> Option<String> {
    let metadata = fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_POINTER_BYTES {
        return None;
    }
    let content = fs::read_to_string(path).ok()?;
    let mut lines = content.lines();
    let line = lines.next()?.to_owned();
    lines.next().is_none().then_some(line)
}

fn push_existing(paths: &mut Vec<PathBuf>, path: PathBuf) {
    if path.exists() && !paths.contains(&path) {
        paths.push(path);
    }
}
