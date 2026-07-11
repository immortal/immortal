//! Bounded discovery and desired-state reconciliation for `immortaldir`.

use std::{
    collections::{BTreeMap, BTreeSet},
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, File, Metadata},
    io::{self, Read},
    num::NonZeroUsize,
    path::{Path, PathBuf},
    time::SystemTime,
};

use crate::{
    config::{ConfigError, MAX_CONFIG_BYTES, ServiceConfig, parse_bytes},
    platform::file_identity,
};

/// Default maximum number of candidate definitions accepted in one directory.
pub const DEFAULT_MAX_DEFINITIONS: usize = 4096;
/// Consecutive authoritative scans required before a missing definition is removed.
pub const DEFAULT_DELETION_CONFIRMATIONS: usize = 2;

/// Resource limits for one authoritative directory scan.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ScanLimits {
    /// Maximum number of top-level `*.yml` candidates.
    pub max_definitions: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            max_definitions: DEFAULT_MAX_DEFINITIONS,
        }
    }
}

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

fn is_candidate(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    !name.starts_with('.') && path.extension().is_some_and(|extension| extension == "yml")
}

fn definition_name(path: &Path) -> Option<String> {
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
    parse_bytes(&bytes).map_err(ScanProblemKind::Config)
}

fn metadata_changed(before: &Metadata, after: &Metadata) -> bool {
    file_identity(before) != file_identity(after)
        || before.len() != after.len()
        || modified(before) != modified(after)
        || !after.is_file()
}

fn modified(metadata: &Metadata) -> Option<SystemTime> {
    metadata.modified().ok()
}

/// Desired-state change computed from two valid semantic snapshots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ReconcileAction {
    /// Definition became enabled and has no current supervisor.
    Start,
    /// Valid normalized configuration changed.
    Restart,
    /// Definition is disabled or was stably removed.
    Stop,
    /// Valid desired and current state are already equivalent.
    Keep,
}

/// Compute semantic desired-state action without relying on mtimes.
#[must_use]
pub fn compare(
    previous: Option<&ServiceConfig>,
    desired: Option<&ServiceConfig>,
) -> ReconcileAction {
    match (previous, desired) {
        (None, Some(config)) if config.enabled => ReconcileAction::Start,
        (Some(_), None) => ReconcileAction::Stop,
        (None, Some(_) | None) => ReconcileAction::Keep,
        (Some(old), Some(new)) if old == new => ReconcileAction::Keep,
        (Some(old), Some(new)) if old.enabled && !new.enabled => ReconcileAction::Stop,
        (Some(old), Some(new)) if !old.enabled && new.enabled => ReconcileAction::Start,
        (Some(_), Some(new)) if !new.enabled => ReconcileAction::Keep,
        (Some(_), Some(_)) => ReconcileAction::Restart,
    }
}

/// Merge a scan into last-known-good desired state.
///
/// Names with invalid replacement files retain their previous valid value.
/// Valid definitions replace prior values. Stable deletion handling remains a
/// caller policy because a single partial scan must not imply deletion.
#[must_use]
pub fn retain_last_known_good(
    previous: &BTreeMap<String, ServiceConfig>,
    scan: &ScanResult,
) -> BTreeMap<String, ServiceConfig> {
    let mut desired = previous.clone();
    desired.extend(
        scan.definitions
            .iter()
            .map(|(name, definition)| (name.clone(), definition.config.clone())),
    );
    desired
}

/// Persistent desired-state view built from authoritative directory scans.
///
/// Valid definitions replace older values immediately. Invalid replacements
/// retain their last-known-good value and count as present. A missing file must
/// remain absent for the configured number of consecutive scans before its
/// desired state is removed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DesiredStateTracker {
    desired: BTreeMap<String, ServiceConfig>,
    absent_scans: BTreeMap<String, usize>,
    deletion_confirmations: NonZeroUsize,
}

impl Default for DesiredStateTracker {
    fn default() -> Self {
        Self::new(NonZeroUsize::new(DEFAULT_DELETION_CONFIRMATIONS).unwrap_or(NonZeroUsize::MIN))
    }
}

impl DesiredStateTracker {
    /// Construct a tracker with an explicit stable-deletion threshold.
    #[must_use]
    pub const fn new(deletion_confirmations: NonZeroUsize) -> Self {
        Self {
            desired: BTreeMap::new(),
            absent_scans: BTreeMap::new(),
            deletion_confirmations,
        }
    }

    /// Current last-known-good desired configurations.
    #[must_use]
    pub const fn desired(&self) -> &BTreeMap<String, ServiceConfig> {
        &self.desired
    }

    /// Apply one complete scan and return one deterministic action per known name.
    ///
    /// Reapplying an identical scan yields `Keep`; a confirmed deletion yields
    /// `Stop` once and disappears from subsequent plans.
    pub fn apply(&mut self, scan: &ScanResult) -> BTreeMap<String, ReconcileAction> {
        let problem_names: BTreeSet<String> = scan
            .problems
            .iter()
            .filter_map(|problem| {
                is_candidate(&problem.path)
                    .then(|| definition_name(&problem.path))
                    .flatten()
            })
            .collect();
        let scan_incomplete = scan.problems.iter().any(|problem| {
            matches!(&problem.kind, ScanProblemKind::Io(_)) && !is_candidate(&problem.path)
        });
        let mut actions = BTreeMap::new();

        for (name, definition) in &scan.definitions {
            let action = compare(self.desired.get(name), Some(&definition.config));
            self.desired.insert(name.clone(), definition.config.clone());
            self.absent_scans.remove(name);
            actions.insert(name.clone(), action);
        }

        for name in &problem_names {
            self.absent_scans.remove(name);
            if self.desired.contains_key(name) {
                actions.entry(name.clone()).or_insert(ReconcileAction::Keep);
            }
        }

        let missing: Vec<String> = self
            .desired
            .keys()
            .filter(|name| !scan.definitions.contains_key(*name) && !problem_names.contains(*name))
            .cloned()
            .collect();
        for name in missing {
            if scan_incomplete {
                actions.insert(name, ReconcileAction::Keep);
                continue;
            }
            let count = self.absent_scans.entry(name.clone()).or_default();
            *count = count.saturating_add(1);
            if *count >= self.deletion_confirmations.get() {
                self.absent_scans.remove(&name);
                self.desired.remove(&name);
                actions.insert(name, ReconcileAction::Stop);
            } else {
                actions.insert(name, ReconcileAction::Keep);
            }
        }
        actions
    }
}

/// Deterministic groups of independent services which may start concurrently.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DependencyPlan {
    /// Ordered waves. Every dependency of a wave appears in an earlier wave.
    pub waves: Vec<Vec<String>>,
}

/// Invalid desired dependency graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DependencyError {
    /// Required definition is missing or explicitly disabled.
    Unavailable {
        /// Service with the requirement.
        service: String,
        /// Missing or disabled requirement.
        dependency: String,
    },
    /// Enabled definitions contain a dependency cycle.
    Cycle {
        /// Sorted services which could not be topologically ordered.
        services: Vec<String>,
    },
}

impl Display for DependencyError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable {
                service,
                dependency,
            } => write!(
                formatter,
                "service `{service}` requires unavailable service `{dependency}`"
            ),
            Self::Cycle { services } => {
                write!(formatter, "dependency cycle among: {}", services.join(", "))
            }
        }
    }
}

impl Error for DependencyError {}

/// Validate enabled-service dependencies and compute concurrent start waves.
///
/// `requires` gates initial starts only. This plan intentionally says nothing
/// about cascading stops after a dependency later becomes unavailable.
///
/// # Errors
///
/// Returns an error for a missing/disabled dependency or any cycle.
pub fn dependency_plan(
    desired: &BTreeMap<String, ServiceConfig>,
) -> Result<DependencyPlan, DependencyError> {
    let enabled: BTreeSet<&str> = desired
        .iter()
        .filter_map(|(name, config)| config.enabled.then_some(name.as_str()))
        .collect();
    let mut remaining_requirements = BTreeMap::new();
    let mut dependents: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();

    for (name, config) in desired.iter().filter(|(_, config)| config.enabled) {
        for dependency in &config.requires {
            if !enabled.contains(dependency.as_str()) {
                return Err(DependencyError::Unavailable {
                    service: name.clone(),
                    dependency: dependency.clone(),
                });
            }
            dependents
                .entry(dependency)
                .or_default()
                .insert(name.as_str());
        }
        remaining_requirements.insert(name.as_str(), config.requires.len());
    }

    let mut ready: Vec<&str> = remaining_requirements
        .iter()
        .filter_map(|(name, count)| (*count == 0).then_some(*name))
        .collect();
    let mut waves = Vec::new();
    let mut scheduled = 0_usize;
    while !ready.is_empty() {
        ready.sort_unstable();
        let wave = std::mem::take(&mut ready);
        scheduled = scheduled.saturating_add(wave.len());
        let mut next = BTreeSet::new();
        for completed in &wave {
            if let Some(children) = dependents.get(completed) {
                for child in children {
                    if let Some(count) = remaining_requirements.get_mut(child) {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            next.insert(*child);
                        }
                    }
                }
            }
        }
        waves.push(wave.into_iter().map(ToOwned::to_owned).collect());
        ready.extend(next);
    }

    if scheduled == remaining_requirements.len() {
        Ok(DependencyPlan { waves })
    } else {
        let services = remaining_requirements
            .into_iter()
            .filter(|(_, count)| *count != 0)
            .map(|(name, _)| name.to_owned())
            .collect();
        Err(DependencyError::Cycle { services })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        error::Error,
        fs, io,
        num::NonZeroUsize,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use super::{
        DependencyError, DesiredStateTracker, ReconcileAction, ScanLimits, ScanProblem,
        ScanProblemKind, ScanResult, canonical_definitions_directory, compare, dependency_plan,
        metadata_changed, retain_last_known_good, scan_directory,
    };
    use crate::config::MAX_CONFIG_BYTES;

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortal-reconcile-{}-{sequence}",
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
    fn scans_only_top_level_visible_yml_files() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(
            directory.path().join("notes.yaml"),
            "version: 2\ncommand: [/bin/false]\n",
        )?;
        fs::write(
            directory.path().join(".hidden.yml"),
            "version: 2\ncommand: [/bin/false]\n",
        )?;
        fs::create_dir(directory.path().join("nested"))?;
        fs::write(
            directory.path().join("nested/worker.yml"),
            "version: 2\ncommand: [/bin/false]\n",
        )?;

        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(scan.definitions.len(), 1);
        assert!(scan.definitions.contains_key("api"));
        assert!(scan.problems.is_empty());
        Ok(())
    }

    #[test]
    fn canonicalizes_a_real_definitions_directory() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let canonical = canonical_definitions_directory(directory.path())?;
        assert!(canonical.is_absolute());
        assert_eq!(canonical, fs::canonicalize(directory.path())?);
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_a_symlinked_definitions_directory() -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let parent = TestDirectory::new()?;
        let target = parent.path().join("target");
        let link = parent.path().join("definitions");
        fs::create_dir(&target)?;
        symlink(&target, &link)?;
        assert!(canonical_definitions_directory(&link).is_err());
        Ok(())
    }

    #[test]
    fn file_identity_detects_same_sized_replacement() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let first = directory.path().join("first.yml");
        let second = directory.path().join("second.yml");
        fs::write(&first, b"1234")?;
        fs::write(&second, b"1234")?;
        assert!(metadata_changed(
            &fs::metadata(first)?,
            &fs::metadata(second)?
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlinks_and_unsafe_names_without_losing_valid_services()
    -> Result<(), Box<dyn Error>> {
        use std::os::unix::fs::symlink;

        let directory = TestDirectory::new()?;
        let target = directory.path().join("target");
        fs::write(&target, "version: 2\ncommand: [/bin/true]\n")?;
        symlink(&target, directory.path().join("linked.yml"))?;
        fs::write(
            directory.path().join("unsafe name.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(
            directory.path().join("valid.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;

        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        assert!(scan.definitions.contains_key("valid"));
        assert!(
            scan.problems
                .iter()
                .any(|problem| matches!(problem.kind, ScanProblemKind::Symlink))
        );
        assert!(
            scan.problems
                .iter()
                .any(|problem| matches!(problem.kind, ScanProblemKind::UnsafeName))
        );
        Ok(())
    }

    #[test]
    fn isolates_invalid_files_and_enforces_definition_limit() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("a.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        fs::write(directory.path().join("b.yml"), "not: [valid\n")?;
        fs::write(
            directory.path().join("c.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;

        let scan = scan_directory(directory.path(), ScanLimits { max_definitions: 2 })?;
        assert!(scan.definitions.len() <= 2);
        assert!(scan.problems.iter().any(|problem| matches!(
            problem.kind,
            ScanProblemKind::Config(_) | ScanProblemKind::DefinitionLimit
        )));
        assert!(
            scan.problems
                .iter()
                .any(|problem| matches!(problem.kind, ScanProblemKind::DefinitionLimit))
        );
        Ok(())
    }

    #[test]
    fn rejects_an_oversized_definition_without_allocating_it() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("large.yml");
        let file = fs::File::create(&path)?;
        file.set_len((MAX_CONFIG_BYTES as u64).saturating_add(1))?;

        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        assert!(scan.definitions.is_empty());
        assert!(scan.problems.iter().any(|problem| matches!(
            problem.kind,
            ScanProblemKind::Config(crate::config::ConfigError::TooLarge { .. })
        )));
        Ok(())
    }

    #[test]
    fn compares_semantics_instead_of_file_metadata() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n",
        )?;
        let first = scan_directory(directory.path(), ScanLimits::default())?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [/bin/true]\n# metadata-only change\n",
        )?;
        let second = scan_directory(directory.path(), ScanLimits::default())?;
        let old = first.definitions.get("api").map(|value| &value.config);
        let new = second.definitions.get("api").map(|value| &value.config);
        assert_eq!(compare(old, new), ReconcileAction::Keep);
        Ok(())
    }

    #[test]
    fn invalid_replacement_retains_last_known_good() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let valid = scan_directory(directory.path(), ScanLimits::default())?;
        let desired = retain_last_known_good(&BTreeMap::default(), &valid);
        fs::write(&path, "version: 2\ncommand: []\n")?;
        let invalid = scan_directory(directory.path(), ScanLimits::default())?;
        let retained = retain_last_known_good(&desired, &invalid);
        assert_eq!(retained, desired);
        Ok(())
    }

    #[test]
    fn desired_tracker_confirms_deletion_and_emits_stop_once() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::new(NonZeroUsize::new(2).ok_or("invalid limit")?);

        let present = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&present).get("api"),
            Some(&ReconcileAction::Start)
        );
        assert!(tracker.desired().contains_key("api"));

        fs::remove_file(&path)?;
        let first_missing = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&first_missing).get("api"),
            Some(&ReconcileAction::Keep)
        );
        assert!(tracker.desired().contains_key("api"));

        let confirmed_missing = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&confirmed_missing).get("api"),
            Some(&ReconcileAction::Stop)
        );
        assert!(!tracker.desired().contains_key("api"));
        assert!(tracker.apply(&confirmed_missing).is_empty());
        Ok(())
    }

    #[test]
    fn desired_tracker_never_treats_invalid_replacement_as_deletion() -> Result<(), Box<dyn Error>>
    {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::default();
        let valid = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&valid).get("api"),
            Some(&ReconcileAction::Start)
        );

        fs::write(&path, "version: 2\ncommand: []\n")?;
        for _ in 0..3 {
            let invalid = scan_directory(directory.path(), ScanLimits::default())?;
            assert_eq!(
                tracker.apply(&invalid).get("api"),
                Some(&ReconcileAction::Keep)
            );
            assert_eq!(
                tracker
                    .desired()
                    .get("api")
                    .map(|config| config.command.as_slice()),
                Some(["/bin/true".to_owned()].as_slice())
            );
        }
        Ok(())
    }

    #[test]
    fn desired_tracker_does_not_confirm_deletion_from_incomplete_scans()
    -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let mut tracker = DesiredStateTracker::default();
        let valid = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&valid).get("api"),
            Some(&ReconcileAction::Start)
        );
        fs::remove_file(path)?;

        for _ in 0..3 {
            let incomplete = ScanResult {
                definitions: BTreeMap::new(),
                problems: vec![ScanProblem {
                    path: directory.path().to_owned(),
                    kind: ScanProblemKind::Io(io::Error::other("injected enumeration failure")),
                }],
            };
            assert_eq!(
                tracker.apply(&incomplete).get("api"),
                Some(&ReconcileAction::Keep)
            );
            assert!(tracker.desired().contains_key("api"));
        }

        let complete = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&complete).get("api"),
            Some(&ReconcileAction::Keep)
        );
        assert_eq!(
            tracker.apply(&complete).get("api"),
            Some(&ReconcileAction::Stop)
        );
        Ok(())
    }

    #[test]
    fn desired_tracker_reports_semantic_restart_and_disabled_stop() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let path = directory.path().join("api.yml");
        let mut tracker = DesiredStateTracker::default();
        fs::write(&path, "version: 2\ncommand: [/bin/true]\n")?;
        let first = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&first).get("api"),
            Some(&ReconcileAction::Start)
        );

        fs::write(&path, "version: 2\ncommand: [/bin/false]\n")?;
        let changed = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&changed).get("api"),
            Some(&ReconcileAction::Restart)
        );
        assert_eq!(
            tracker.apply(&changed).get("api"),
            Some(&ReconcileAction::Keep)
        );

        fs::write(&path, "version: 2\nenabled: false\ncommand: [/bin/false]\n")?;
        let disabled = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&disabled).get("api"),
            Some(&ReconcileAction::Stop)
        );
        assert_eq!(
            tracker.apply(&disabled).get("api"),
            Some(&ReconcileAction::Keep)
        );

        fs::write(&path, "version: 2\ncommand: [/bin/false]\n")?;
        let reenabled = scan_directory(directory.path(), ScanLimits::default())?;
        assert_eq!(
            tracker.apply(&reenabled).get("api"),
            Some(&ReconcileAction::Start)
        );
        Ok(())
    }

    #[test]
    fn dependency_plan_groups_independent_services() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("database.yml"),
            "version: 2\ncommand: [database]\n",
        )?;
        fs::write(
            directory.path().join("cache.yml"),
            "version: 2\ncommand: [cache]\n",
        )?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [api]\nrequires: [database, cache]\n",
        )?;
        fs::write(
            directory.path().join("worker.yml"),
            "version: 2\ncommand: [worker]\nrequires: [database]\n",
        )?;
        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        let desired = retain_last_known_good(&BTreeMap::default(), &scan);

        assert_eq!(
            dependency_plan(&desired)?.waves,
            [
                vec!["cache".to_owned(), "database".to_owned()],
                vec!["api".to_owned(), "worker".to_owned()],
            ]
        );
        Ok(())
    }

    #[test]
    fn dependency_plan_rejects_missing_disabled_and_cycles() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        fs::write(
            directory.path().join("api.yml"),
            "version: 2\ncommand: [api]\nrequires: [database]\n",
        )?;
        let scan = scan_directory(directory.path(), ScanLimits::default())?;
        let mut desired = retain_last_known_good(&BTreeMap::default(), &scan);
        assert!(matches!(
            dependency_plan(&desired),
            Err(DependencyError::Unavailable { .. })
        ));

        let mut database = desired
            .get("api")
            .ok_or_else(|| io::Error::other("api fixture missing"))?
            .clone();
        database.enabled = false;
        database.command = vec!["database".to_owned()];
        database.requires.clear();
        desired.insert("database".to_owned(), database);
        assert!(matches!(
            dependency_plan(&desired),
            Err(DependencyError::Unavailable { .. })
        ));

        let api = desired
            .get_mut("api")
            .ok_or_else(|| io::Error::other("api fixture missing"))?;
        api.requires = vec!["worker".to_owned()];
        let mut worker = api.clone();
        worker.command = vec!["worker".to_owned()];
        worker.requires = vec!["api".to_owned()];
        desired.insert("worker".to_owned(), worker);
        assert_eq!(
            dependency_plan(&desired),
            Err(DependencyError::Cycle {
                services: vec!["api".to_owned(), "worker".to_owned()]
            })
        );
        Ok(())
    }
}
