//! Cross-module reconciliation contract tests.

use std::{
    collections::BTreeMap,
    error::Error,
    fs::{self, File},
    io,
    num::NonZeroUsize,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use super::{
    DefinitionSnapshots, DependencyError, DesiredStateTracker, LaunchConcurrency,
    MAX_CONCURRENT_LAUNCHES, MAX_TRACKER_STATE_BYTES, ReconcileAction, ScanLimits, ScanProblem,
    ScanProblemKind, ScanResult, TRACKER_STATE_FILE, UnresolvableService,
    canonical_definitions_directory, compare, dependency_plan, metadata_changed,
    retain_last_known_good, scan_directory,
};
use crate::config::MAX_CONFIG_BYTES;

static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

#[test]
fn launch_concurrency_rejects_zero_and_values_above_the_hard_bound() {
    assert!(LaunchConcurrency::new(0).is_err());
    assert_eq!(
        LaunchConcurrency::new(MAX_CONCURRENT_LAUNCHES).map(LaunchConcurrency::get),
        Ok(MAX_CONCURRENT_LAUNCHES)
    );
    assert!(LaunchConcurrency::new(MAX_CONCURRENT_LAUNCHES + 1).is_err());
}

struct TestDirectory(PathBuf);

impl TestDirectory {
    fn new() -> Result<Self, Box<dyn Error>> {
        let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = Path::new("/tmp").join(format!(
            "immortal-reconcile-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path)?;
        // Definitions directories are a trust boundary, so fixtures must not
        // inherit a group-writable umask.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
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

#[test]
fn normalized_snapshot_is_owner_only_and_atomically_replaceable() -> Result<(), Box<dyn Error>> {
    let runtime = TestDirectory::new()?;
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
    let snapshots = DefinitionSnapshots::open(runtime.path())?;
    let first = crate::config::parse_str("version: 2\ncommand: [/bin/true]\n")?;
    let path = snapshots.publish("api", &first)?;
    assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
    assert_eq!(crate::config::parse_file(&path)?, first);

    let second = crate::config::parse_str("version: 2\ncommand: [/bin/false]\n")?;
    assert_eq!(snapshots.publish("api", &second)?, path);
    assert_eq!(crate::config::parse_file(&path)?, second);
    snapshots.record_applied("api", &first)?;
    assert_eq!(snapshots.load_applied("api")?, Some(first));
    snapshots.remove_applied("api")?;
    assert_eq!(snapshots.load_applied("api")?, None);
    assert!(
        fs::read_dir(runtime.path().join(".definitions"))?.all(|entry| {
            entry.is_ok_and(|entry| entry.path().extension().is_none_or(|value| value != "tmp"))
        })
    );
    Ok(())
}

#[test]
fn tracker_checkpoint_retries_confirmed_deletion_after_restart() -> Result<(), Box<dyn Error>> {
    let runtime = TestDirectory::new()?;
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
    let definitions = TestDirectory::new()?;
    let definition = definitions.path().join("api.yml");
    fs::write(&definition, "version: 2\ncommand: [/bin/true]\n")?;
    let snapshots = DefinitionSnapshots::open(runtime.path())?;
    let config = crate::config::parse_file(&definition)?;
    snapshots.record_applied("api", &config)?;
    assert_eq!(
        snapshots.load_tracker()?.desired().get("api"),
        Some(&config)
    );

    let mut tracker = DesiredStateTracker::default();
    let present = scan_directory(definitions.path(), ScanLimits::default())?;
    assert_eq!(
        tracker.apply(&present).get("api"),
        Some(&ReconcileAction::Start)
    );
    snapshots.record_tracker(&tracker)?;

    fs::remove_file(&definition)?;
    let missing = scan_directory(definitions.path(), ScanLimits::default())?;
    assert_eq!(
        tracker.apply(&missing).get("api"),
        Some(&ReconcileAction::Keep)
    );
    snapshots.record_tracker(&tracker)?;

    let mut restored = snapshots.load_tracker()?;
    assert!(restored.desired().contains_key("api"));
    assert_eq!(
        restored.apply(&missing).get("api"),
        Some(&ReconcileAction::Stop)
    );
    snapshots.record_tracker(&restored)?;

    let mut confirmed = snapshots.load_tracker()?;
    assert_eq!(
        confirmed.apply(&missing).get("api"),
        Some(&ReconcileAction::Stop)
    );
    snapshots.remove_applied("api")?;
    confirmed.acknowledge_deletion("api");
    snapshots.record_tracker(&confirmed)?;
    assert_eq!(snapshots.load_tracker()?, DesiredStateTracker::default());

    let state = snapshots.directory.join(TRACKER_STATE_FILE);
    assert_eq!(fs::metadata(state)?.permissions().mode() & 0o777, 0o600);
    Ok(())
}

#[test]
fn tracker_checkpoint_rejects_malformed_unbounded_and_unsafe_state() -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::symlink;

    let runtime = TestDirectory::new()?;
    fs::set_permissions(runtime.path(), fs::Permissions::from_mode(0o700))?;
    let snapshots = DefinitionSnapshots::open(runtime.path())?;
    let state = snapshots.directory.join(TRACKER_STATE_FILE);

    fs::write(&state, "version\t1\n")?;
    fs::set_permissions(&state, fs::Permissions::from_mode(0o644))?;
    assert!(snapshots.load_tracker().is_err());
    fs::set_permissions(&state, fs::Permissions::from_mode(0o600))?;

    for malformed in [
        "",
        "version\t2\n",
        "version\t1\nmissing-count\n",
        "version\t1\nunsafe/name\t0\n",
        "version\t1\napi\t0\napi\t1\n",
        "version\t1\napi\t3\n",
    ] {
        fs::write(&state, malformed)?;
        assert!(snapshots.load_tracker().is_err(), "accepted {malformed:?}");
    }

    fs::write(&state, [0xff, 0xfe])?;
    assert!(snapshots.load_tracker().is_err());
    let file = File::create(&state)?;
    file.set_len((MAX_TRACKER_STATE_BYTES as u64).saturating_add(1))?;
    assert!(snapshots.load_tracker().is_err());

    drop(file);
    fs::remove_file(&state)?;
    let target = runtime.path().join("tracker-target");
    fs::write(&target, "version\t1\n")?;
    fs::set_permissions(&target, fs::Permissions::from_mode(0o600))?;
    symlink(target, &state)?;
    assert!(snapshots.load_tracker().is_err());
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
fn rejects_symlinks_and_unsafe_names_without_losing_valid_services() -> Result<(), Box<dyn Error>> {
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
fn desired_tracker_never_treats_invalid_replacement_as_deletion() -> Result<(), Box<dyn Error>> {
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
fn desired_tracker_does_not_confirm_deletion_from_incomplete_scans() -> Result<(), Box<dyn Error>> {
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

    let plan = dependency_plan(&desired);
    assert_eq!(
        plan.waves,
        [
            vec!["cache".to_owned(), "database".to_owned()],
            vec!["api".to_owned(), "worker".to_owned()],
        ]
    );
    assert!(plan.unresolvable.is_empty());
    Ok(())
}

#[test]
fn dependency_plan_isolates_missing_disabled_and_cyclic_services() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: [api]\nrequires: [database]\n",
    )?;
    fs::write(
        directory.path().join("web.yml"),
        "version: 2\ncommand: [web]\n",
    )?;
    let scan = scan_directory(directory.path(), ScanLimits::default())?;
    let mut desired = retain_last_known_good(&BTreeMap::default(), &scan);

    // A missing requirement excludes only its dependent; `web` still starts.
    let plan = dependency_plan(&desired);
    assert_eq!(plan.waves, [vec!["web".to_owned()]]);
    assert_eq!(
        plan.unresolvable,
        [UnresolvableService {
            service: "api".to_owned(),
            reason: DependencyError::Unavailable {
                dependency: "database".to_owned()
            },
        }]
    );

    // A disabled requirement is treated exactly like a missing one.
    let mut database = desired
        .get("api")
        .ok_or_else(|| io::Error::other("api fixture missing"))?
        .clone();
    database.enabled = false;
    database.command = vec!["database".to_owned()];
    database.requires.clear();
    desired.insert("database".to_owned(), database);
    let plan = dependency_plan(&desired);
    assert_eq!(plan.waves, [vec!["web".to_owned()]]);
    assert_eq!(
        plan.unresolvable,
        [UnresolvableService {
            service: "api".to_owned(),
            reason: DependencyError::Unavailable {
                dependency: "database".to_owned()
            },
        }]
    );

    // A transitive dependent of an unresolvable service is excluded too, so it
    // never waits in an unreachable wave.
    let mut edge = desired
        .get("web")
        .ok_or_else(|| io::Error::other("web fixture missing"))?
        .clone();
    edge.command = vec!["edge".to_owned()];
    edge.requires = vec!["api".to_owned()];
    desired.insert("edge".to_owned(), edge);
    let plan = dependency_plan(&desired);
    assert_eq!(plan.waves, [vec!["web".to_owned()]]);
    assert_eq!(
        plan.unresolvable,
        [
            UnresolvableService {
                service: "api".to_owned(),
                reason: DependencyError::Unavailable {
                    dependency: "database".to_owned()
                },
            },
            UnresolvableService {
                service: "edge".to_owned(),
                reason: DependencyError::Blocked {
                    dependency: "api".to_owned()
                },
            },
        ]
    );
    Ok(())
}

#[test]
fn dependency_plan_isolates_cycles_and_self_requirements() -> Result<(), Box<dyn Error>> {
    let directory = TestDirectory::new()?;
    fs::write(
        directory.path().join("api.yml"),
        "version: 2\ncommand: [api]\nrequires: [worker]\n",
    )?;
    fs::write(
        directory.path().join("web.yml"),
        "version: 2\ncommand: [web]\n",
    )?;
    let scan = scan_directory(directory.path(), ScanLimits::default())?;
    let mut desired = retain_last_known_good(&BTreeMap::default(), &scan);
    let mut worker = desired
        .get("api")
        .ok_or_else(|| io::Error::other("api fixture missing"))?
        .clone();
    worker.command = vec!["worker".to_owned()];
    worker.requires = vec!["api".to_owned()];
    desired.insert("worker".to_owned(), worker);

    let cycle = vec!["api".to_owned(), "worker".to_owned()];
    let plan = dependency_plan(&desired);
    assert_eq!(plan.waves, [vec!["web".to_owned()]]);
    assert_eq!(
        plan.unresolvable,
        [
            UnresolvableService {
                service: "api".to_owned(),
                reason: DependencyError::Cycle {
                    services: cycle.clone()
                },
            },
            UnresolvableService {
                service: "worker".to_owned(),
                reason: DependencyError::Cycle { services: cycle },
            },
        ]
    );

    // A service requiring itself is a one-member cycle, not a fatal scan error.
    desired.remove("worker");
    let api = desired
        .get_mut("api")
        .ok_or_else(|| io::Error::other("api fixture missing"))?;
    api.requires = vec!["api".to_owned()];
    let plan = dependency_plan(&desired);
    assert_eq!(plan.waves, [vec!["web".to_owned()]]);
    assert_eq!(
        plan.unresolvable,
        [UnresolvableService {
            service: "api".to_owned(),
            reason: DependencyError::Cycle {
                services: vec!["api".to_owned()]
            },
        }]
    );
    Ok(())
}

/// Regression: anyone who can write here chooses what every service runs.
///
/// The directory validated only its type and identity, so a group- or
/// world-writable definitions directory was accepted even though the per-file
/// checks prove only that a file did not change while being read — never that
/// an untrusted principal was unable to place it.
#[test]
fn rejects_a_writable_definitions_directory() -> Result<(), Box<dyn Error>> {
    for mode in [0o775, 0o757, 0o777, 0o1777] {
        let directory = TestDirectory::new()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode))?;
        assert!(
            canonical_definitions_directory(directory.path()).is_err(),
            "mode {mode:o} must be rejected"
        );
        assert!(
            scan_directory(directory.path(), ScanLimits::default()).is_err(),
            "mode {mode:o} must be rejected on the scan path"
        );
    }
    Ok(())
}

/// An owner-only directory owned by the effective user stays usable.
#[test]
fn accepts_an_owner_only_definitions_directory() -> Result<(), Box<dyn Error>> {
    for mode in [0o700, 0o750, 0o755] {
        let directory = TestDirectory::new()?;
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(mode))?;
        assert_eq!(
            canonical_definitions_directory(directory.path())?,
            fs::canonicalize(directory.path())?,
            "mode {mode:o} must be accepted"
        );
    }
    Ok(())
}
