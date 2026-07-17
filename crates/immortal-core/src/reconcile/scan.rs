//! Stable, bounded discovery of desired service definition files.
//!
//! Scans canonicalize the definitions directory once, enumerate only visible
//! top-level `*.yml` candidates, and isolate per-file failures so one malformed
//! service cannot hide other valid desired state. File contents are read through
//! metadata checks before and after parsing to reject symlinks, replacements, and
//! size changes across the trust boundary.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File, Metadata},
    io::{self, Read},
    path::{Path, PathBuf},
    time::SystemTime,
};

use super::limits::ScanLimits;
use crate::{
    config::{ConfigError, MAX_CONFIG_BYTES, ServiceConfig, parse_bytes_at},
    platform::file_identity,
};

/// One valid desired service definition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Definition {
    /// Safe filename stem used as service identity.
    pub name: String,
    /// Source file observed during this scan.
    pub path: PathBuf,
    /// Parsed and normalized service configuration.
    pub config: ServiceConfig,
}

/// Non-fatal problem isolated to one scan or candidate.
#[derive(Debug)]
pub struct ScanProblem {
    /// Candidate involved, or the directory for a global limit.
    pub path: PathBuf,
    /// Stable problem category.
    pub kind: ScanProblemKind,
}

/// Stable directory-scan problem category.
#[derive(Debug)]
pub enum ScanProblemKind {
    /// Candidate count exceeded [`ScanLimits::max_definitions`].
    DefinitionLimit,
    /// Candidate service name is unsafe.
    UnsafeName,
    /// More than one candidate resolved to the same service identity.
    DuplicateName,
    /// Candidate is a symbolic link.
    Symlink,
    /// Candidate is not a regular file.
    NotRegular,
    /// Candidate could not be opened, read, or inspected.
    Io(io::Error),
    /// Candidate changed while its snapshot was being read.
    ChangedDuringRead,
    /// Candidate configuration is oversized, malformed, or invalid.
    Config(ConfigError),
}

/// Result of one complete authoritative directory scan.
#[derive(Debug, Default)]
pub struct ScanResult {
    /// Valid definitions keyed by safe service name.
    pub definitions: BTreeMap<String, Definition>,
    /// Isolated problems which do not invalidate other services.
    pub problems: Vec<ScanProblem>,
}

/// Failure to inspect the definitions directory itself.
#[derive(Debug)]
pub struct ScanError(io::Error);

impl Display for ScanError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unable to scan definitions directory: {}",
            self.0
        )
    }
}

impl Error for ScanError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}
/// Scan top-level, non-hidden, regular `*.yml` definitions with bounded reads.
///
/// An invalid candidate is returned in [`ScanResult::problems`] while other
/// valid services remain available. Filesystem notifications should call this
/// function; they must not be interpreted as desired-state mutations directly.
///
/// # Errors
///
/// Returns an error only when the definitions directory itself cannot be read.
pub fn scan_directory(directory: &Path, limits: ScanLimits) -> Result<ScanResult, ScanError> {
    let directory = canonical_definitions_directory(directory)?;
    let entries = fs::read_dir(&directory).map_err(ScanError)?;
    let mut candidates = Vec::new();
    let mut problems = Vec::new();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                problems.push(ScanProblem {
                    path: directory.clone(),
                    kind: ScanProblemKind::Io(error),
                });
                continue;
            }
        };
        let path = entry.path();
        if !is_candidate(&path) {
            continue;
        }
        if candidates.len() >= limits.max_definitions {
            problems.push(ScanProblem {
                path,
                kind: ScanProblemKind::DefinitionLimit,
            });
            continue;
        }
        candidates.push(path);
    }
    candidates.sort();

    let mut definitions = BTreeMap::new();
    for path in candidates {
        let Some(name) = definition_name(&path) else {
            problems.push(ScanProblem {
                path,
                kind: ScanProblemKind::UnsafeName,
            });
            continue;
        };
        match read_definition(&path) {
            Ok(config) => {
                if definitions.contains_key(&name) {
                    problems.push(ScanProblem {
                        path,
                        kind: ScanProblemKind::DuplicateName,
                    });
                } else {
                    definitions.insert(name.clone(), Definition { name, path, config });
                }
            }
            Err(kind) => problems.push(ScanProblem { path, kind }),
        }
    }

    Ok(ScanResult {
        definitions,
        problems,
    })
}

/// Resolve a definitions directory once and reject a symlink or non-directory
/// final component.
///
/// # Errors
///
/// Returns an error when the path cannot be inspected or canonicalized, is not
/// a real directory, or changes identity during validation.
pub fn canonical_definitions_directory(directory: &Path) -> Result<PathBuf, ScanError> {
    let before = fs::symlink_metadata(directory).map_err(ScanError)?;
    if before.file_type().is_symlink() || !before.is_dir() {
        return Err(ScanError(io::Error::new(
            io::ErrorKind::InvalidInput,
            "definitions path must be a real directory, not a symlink",
        )));
    }
    let canonical = fs::canonicalize(directory).map_err(ScanError)?;
    let after = fs::metadata(&canonical).map_err(ScanError)?;
    if !after.is_dir() || file_identity(&before) != file_identity(&after) {
        return Err(ScanError(io::Error::new(
            io::ErrorKind::InvalidInput,
            "definitions directory changed during validation",
        )));
    }
    Ok(canonical)
}

pub(in crate::reconcile) fn is_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.starts_with('.') && path.extension().is_some_and(|extension| extension == "yml")
}

pub(in crate::reconcile) fn definition_name(path: &Path) -> Option<String> {
    let name = path.file_stem()?.to_str()?;
    let safe = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'));
    safe.then(|| name.to_owned())
}

fn read_definition(path: &Path) -> Result<ServiceConfig, ScanProblemKind> {
    let path_before = fs::symlink_metadata(path).map_err(ScanProblemKind::Io)?;
    if path_before.file_type().is_symlink() {
        return Err(ScanProblemKind::Symlink);
    }
    if !path_before.is_file() {
        return Err(ScanProblemKind::NotRegular);
    }

    let mut file = File::open(path).map_err(ScanProblemKind::Io)?;
    let before = file.metadata().map_err(ScanProblemKind::Io)?;
    if !before.is_file() {
        return Err(ScanProblemKind::NotRegular);
    }
    if before.len() > MAX_CONFIG_BYTES as u64 {
        return Err(ScanProblemKind::Config(ConfigError::TooLarge {
            actual: before.len(),
        }));
    }

    let mut bytes = Vec::with_capacity(usize::try_from(before.len()).unwrap_or(0));
    (&mut file)
        .take((MAX_CONFIG_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(ScanProblemKind::Io)?;
    let file_after = file.metadata().map_err(ScanProblemKind::Io)?;
    let path_after = fs::symlink_metadata(path).map_err(ScanProblemKind::Io)?;
    if path_after.file_type().is_symlink()
        || metadata_changed(&before, &file_after)
        || metadata_changed(&before, &path_after)
    {
        return Err(ScanProblemKind::ChangedDuringRead);
    }
    parse_bytes_at(&bytes, path).map_err(ScanProblemKind::Config)
}

pub(in crate::reconcile) fn metadata_changed(before: &Metadata, after: &Metadata) -> bool {
    file_identity(before) != file_identity(after)
        || before.len() != after.len()
        || modified(before) != modified(after)
        || !after.is_file()
}

fn modified(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}
