//! Runtime-root preparation, exclusive service ownership, and safe discovery.
//!
//! Direct and config launches first resolve a bounded service identity, then
//! prepare the effective user's owner-only root before daemonization. Execution
//! acquires the service lock before removing a proven stale socket and retains
//! that ownership through broker cleanup. Control and reconciliation perform
//! read-only bounded discovery through the same canonical-path, ownership,
//! permission, and no-symlink contract; malformed entries are isolated rather
//! than mutated.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, DirBuilder, File, Metadata, OpenOptions, TryLockError},
    io,
    os::unix::{
        ffi::OsStrExt,
        fs::{DirBuilderExt, FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
};

use nix::unistd::{Uid, User};

use crate::service_name::is_safe_service_name;

/// Control socket filename inside each service runtime directory.
pub const CONTROL_SOCKET_NAME: &str = "immortal.sock";
/// Advisory supervisor lock held for the complete runtime-directory lifetime.
pub const SUPERVISOR_LOCK_NAME: &str = "supervisor.lock";
/// Maximum safely discoverable services in one runtime root.
pub const MAX_RUNTIME_SERVICES: usize = 4096;
/// Portable maximum pathname bytes for a Unix control socket, excluding NUL.
pub const MAX_CONTROL_SOCKET_PATH_BYTES: usize = 103;

#[cfg(target_os = "linux")]
const SYSTEM_RUNTIME_ROOT: &str = "/run/immortal";
#[cfg(target_os = "freebsd")]
const SYSTEM_RUNTIME_ROOT: &str = "/var/run/immortal";
// launchd has no pre-start hook, so a macOS system root has to survive reboot
// rather than be recreated by the job which uses it.
#[cfg(target_os = "macos")]
const SYSTEM_RUNTIME_ROOT: &str = "/var/db/immortal/run";

/// Return the platform system-supervisor discovery root.
#[must_use]
pub fn system_runtime_root() -> &'static Path {
    Path::new(SYSTEM_RUNTIME_ROOT)
}

/// Resolve the effective user's portable supervisor discovery root.
///
/// An absolute nonempty `HOME` is preferred. If it is unavailable, the
/// effective account database entry supplies the home directory. Existing home
/// aliases are canonicalized before `.immortal` is appended. The resolved home
/// must belong to the effective UID and not be group/world writable, while the
/// runtime root entry itself remains subject to the no-symlink contract.
///
/// # Errors
///
/// Returns an error when neither source provides an absolute, canonicalizable,
/// effective-UID-owned home directory with safe write permissions.
pub fn user_runtime_root() -> io::Result<PathBuf> {
    if let Some(home) = std::env::var_os("HOME").filter(|home| !home.is_empty()) {
        let home = PathBuf::from(home);
        if home.is_absolute() {
            return runtime_root_from_home(&home);
        }
    }
    let user = User::from_uid(Uid::effective())?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "effective user has no account database entry",
        )
    })?;
    if !user.dir.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "effective user home directory is not absolute",
        ));
    }
    runtime_root_from_home(&user.dir)
}

fn runtime_root_from_home(home: &Path) -> io::Result<PathBuf> {
    runtime_root_from_home_for_owner(home, Uid::effective().as_raw())
}

fn runtime_root_from_home_for_owner(home: &Path, expected_owner: u32) -> io::Result<PathBuf> {
    let home = fs::canonicalize(home)?;
    let metadata = fs::symlink_metadata(&home)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "effective user home must be a directory",
        ));
    }
    if metadata.uid() != expected_owner {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "effective user home owner differs from the effective user",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "effective user home must not be group or world writable",
        ));
    }
    Ok(home.join(".immortal"))
}

/// Prepare the effective user's runtime root and resolve one service directory.
///
/// The `$HOME/.immortal` root is created with mode `0700` when absent. Existing
/// roots must be canonical real directories owned by the effective UID with
/// exactly that mode. The returned service path is not created until runtime
/// ownership is acquired.
///
/// # Errors
///
/// Returns an error for an unsafe service name or control-socket pathname,
/// unavailable or untrusted home directory, symlinked or noncanonical root,
/// ownership or mode mismatch, or creation failure.
pub fn prepare_user_service_directory(service_name: &str) -> io::Result<PathBuf> {
    let root = user_runtime_root()?;
    prepare_user_service_directory_at(&root, service_name)
}

fn prepare_user_service_directory_at(root: &Path, service_name: &str) -> io::Result<PathBuf> {
    if !is_safe_service_name(service_name) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsafe service name",
        ));
    }
    let directory = root.join(service_name);
    validate_control_socket_path(&directory)?;
    create_user_runtime_root(root)?;
    Ok(directory)
}

fn create_user_runtime_root(root: &Path) -> io::Result<()> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "user runtime root must be absolute",
        ));
    }
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    match builder.create(root) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = validate_root(root)?;
    if metadata.uid() != Uid::effective().as_raw() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "user runtime root owner differs from the effective user",
        ));
    }
    if metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "user runtime root mode must be 0700",
        ));
    }
    Ok(())
}

/// Exclusive ownership of one service runtime directory.
#[derive(Debug)]
pub struct RuntimeOwner {
    directory: PathBuf,
    socket: PathBuf,
    _lock: File,
}

impl RuntimeOwner {
    /// Acquire one safe service directory and its nonblocking advisory lock.
    ///
    /// The parent runtime root must already exist and satisfy discovery policy.
    /// The service directory is created with mode `0700` when absent. A stale
    /// socket is removed only after the lock is held and only when it is an
    /// owner-matching mode-`0600` Unix socket.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe or non-portable socket paths, ownership/mode
    /// mismatches, another live lock holder, or stale entries which cannot be
    /// proven safe to remove.
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        let (root, _name) = validate_service_path(directory)?;
        validate_control_socket_path(directory)?;
        let root_metadata = validate_root(root)?;
        create_service_directory(directory)?;
        let directory_metadata = validate_service_directory(directory, &root_metadata)?;
        let lock = acquire_lock(directory, &directory_metadata)?;
        let socket = directory.join(CONTROL_SOCKET_NAME);
        remove_owned_stale_socket(&socket, &directory_metadata)?;
        Ok(Self {
            directory: directory.to_owned(),
            socket,
            _lock: lock,
        })
    }

    /// Canonical service directory held by this owner.
    #[must_use]
    pub fn directory(&self) -> &Path {
        &self.directory
    }

    /// Control socket path made safe for a new listener.
    #[must_use]
    pub fn socket(&self) -> &Path {
        &self.socket
    }
}

fn validate_control_socket_path(directory: &Path) -> io::Result<()> {
    let length = directory
        .join(CONTROL_SOCKET_NAME)
        .as_os_str()
        .as_bytes()
        .len();
    if length > MAX_CONTROL_SOCKET_PATH_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!(
                "control socket path is {length} bytes; portable maximum is {MAX_CONTROL_SOCKET_PATH_BYTES}"
            ),
        ));
    }
    Ok(())
}

fn validate_service_path(directory: &Path) -> io::Result<(&Path, &str)> {
    if !directory.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service runtime directory must be absolute",
        ));
    }
    let root = directory.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "service runtime directory has no parent root",
        )
    })?;
    let name = directory
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| is_safe_service_name(name))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "unsafe service name"))?;
    Ok((root, name))
}

fn create_service_directory(directory: &Path) -> io::Result<()> {
    let mut builder = DirBuilder::new();
    builder.mode(0o700);
    match builder.create(directory) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_service_directory(directory: &Path, root: &Metadata) -> io::Result<Metadata> {
    if fs::canonicalize(directory)? != directory {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service runtime directory must be canonical and contain no symlink",
        ));
    }
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "service runtime path must be a real directory",
        ));
    }
    if metadata.mode() & 0o777 != 0o700 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "service runtime directory mode must be 0700",
        ));
    }
    if metadata.uid() != root.uid() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime root and service directory owners differ",
        ));
    }
    Ok(metadata)
}

fn acquire_lock(directory: &Path, owner: &Metadata) -> io::Result<File> {
    let path = directory.join(SUPERVISOR_LOCK_NAME);
    validate_lock_path_before_open(&path, owner)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create(true)
        .mode(0o600)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let lock = options.open(&path)?;
    lock.set_permissions(fs::Permissions::from_mode(0o600))?;
    validate_lock_identity(&path, &lock, owner)?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "another supervisor owns the service runtime directory",
            ));
        }
        Err(TryLockError::Error(error)) => return Err(error),
    }
    validate_lock_identity(&path, &lock, owner)?;
    Ok(lock)
}

fn validate_lock_path_before_open(path: &Path, owner: &Metadata) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata)
            if !metadata.file_type().is_symlink()
                && metadata.is_file()
                && metadata.uid() == owner.uid() =>
        {
            Ok(())
        }
        Ok(_) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe existing supervisor lock file",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn validate_lock_identity(path: &Path, lock: &File, owner: &Metadata) -> io::Result<()> {
    let path_metadata = fs::symlink_metadata(path)?;
    let file_metadata = lock.metadata()?;
    if path_metadata.file_type().is_symlink()
        || !path_metadata.is_file()
        || path_metadata.uid() != owner.uid()
        || path_metadata.mode() & 0o777 != 0o600
        || path_metadata.dev() != file_metadata.dev()
        || path_metadata.ino() != file_metadata.ino()
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "unsafe supervisor lock file",
        ));
    }
    Ok(())
}

fn remove_owned_stale_socket(socket: &Path, owner: &Metadata) -> io::Result<()> {
    let metadata = match fs::symlink_metadata(socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_socket()
        || metadata.uid() != owner.uid()
        || metadata.mode() & 0o777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "refusing to remove an unsafe stale control endpoint",
        ));
    }
    fs::remove_file(socket)
}

/// One permission-validated runtime service endpoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RuntimeService {
    /// Safe service name.
    pub name: String,
    /// Canonical service runtime directory.
    pub directory: PathBuf,
    /// Unix control socket.
    pub socket: PathBuf,
    /// UID owning both directory and socket.
    pub owner_uid: u32,
}

/// Isolated malformed entry ignored during discovery.
#[derive(Debug)]
pub struct DiscoveryProblem {
    /// Entry involved in the problem.
    pub path: PathBuf,
    /// Stable problem category.
    pub kind: DiscoveryProblemKind,
}

/// Stable malformed runtime-entry category.
#[derive(Debug)]
pub enum DiscoveryProblemKind {
    /// Entry name cannot safely identify a service.
    UnsafeName,
    /// Entry or socket is a symbolic link.
    Symlink,
    /// Entry is not a directory or endpoint is not a socket.
    WrongType,
    /// Directory or socket permissions permit unsafe mutation/access.
    UnsafePermissions,
    /// Runtime root, service directory, and socket owners disagree.
    OwnerMismatch,
    /// Required control socket is absent.
    MissingSocket,
    /// Metadata inspection failed.
    Io(io::Error),
    /// Service count exceeded [`MAX_RUNTIME_SERVICES`].
    ServiceLimit,
}

/// Complete read-only discovery result.
#[derive(Debug, Default)]
pub struct DiscoveryResult {
    /// Valid endpoints keyed by service name.
    pub services: BTreeMap<String, RuntimeService>,
    /// Malformed entries ignored without mutation.
    pub problems: Vec<DiscoveryProblem>,
}

/// Runtime root itself is absent or unsafe.
#[derive(Debug)]
pub struct RuntimeRootError(io::Error);

impl Display for RuntimeRootError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid runtime root: {}", self.0)
    }
}

impl Error for RuntimeRootError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// Discover safe top-level `SERVICE/immortal.sock` endpoints without mutation.
///
/// # Errors
///
/// Returns an error when the runtime root is noncanonical, a symlink, not a
/// directory, writable by group/other, or cannot be read.
pub fn discover(root: &Path) -> Result<DiscoveryResult, RuntimeRootError> {
    discover_with_owner(root, None)
}

/// Discover endpoints below a root owned by the effective user.
///
/// This adds a root-owner check to [`discover`], preventing a forged `HOME`
/// from redirecting automatic user discovery into another account's tree.
///
/// # Errors
///
/// Returns the same errors as [`discover`] and rejects roots whose owner does
/// not match the effective UID.
pub fn discover_user(root: &Path) -> Result<DiscoveryResult, RuntimeRootError> {
    discover_with_owner(root, Some(Uid::effective().as_raw()))
}

fn discover_with_owner(
    root: &Path,
    expected_owner: Option<u32>,
) -> Result<DiscoveryResult, RuntimeRootError> {
    let root_metadata = validate_root(root).map_err(RuntimeRootError)?;
    if expected_owner.is_some_and(|owner| root_metadata.uid() != owner) {
        return Err(RuntimeRootError(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "user runtime root owner differs from the effective user",
        )));
    }
    let entries = fs::read_dir(root).map_err(RuntimeRootError)?;
    let mut result = DiscoveryResult::default();

    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                result.problems.push(DiscoveryProblem {
                    path: root.to_owned(),
                    kind: DiscoveryProblemKind::Io(error),
                });
                continue;
            }
        };
        if result.services.len() >= MAX_RUNTIME_SERVICES {
            result.problems.push(DiscoveryProblem {
                path: entry.path(),
                kind: DiscoveryProblemKind::ServiceLimit,
            });
            continue;
        }
        inspect_entry(&entry.path(), &root_metadata, &mut result);
    }
    Ok(result)
}

/// Report whether a validated service directory still has a live lock owner.
///
/// This probes the advisory lock itself rather than PID files or socket
/// presence. Acquiring an unlocked probe is immediately undone when the local
/// file handle is dropped; no runtime entry is created or removed.
///
/// # Errors
///
/// Returns an error for an unsafe root/service/lock entry or lock I/O failure.
pub fn supervisor_is_active(directory: &Path) -> io::Result<bool> {
    let (root, _name) = validate_service_path(directory)?;
    let root_metadata = validate_root(root)?;
    let directory_metadata = match fs::symlink_metadata(directory) {
        Ok(_) => validate_service_directory(directory, &root_metadata)?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    let path = directory.join(SUPERVISOR_LOCK_NAME);
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let lock = match options.open(&path) {
        Ok(lock) => lock,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    validate_lock_identity(&path, &lock, &directory_metadata)?;
    match lock.try_lock() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn validate_root(root: &Path) -> io::Result<Metadata> {
    if !root.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime root must be absolute",
        ));
    }
    if fs::canonicalize(root)? != root {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime root must be canonical and contain no symlink",
        ));
    }
    let metadata = fs::symlink_metadata(root)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "runtime root must be a real directory",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "runtime root must not be group or world writable",
        ));
    }
    Ok(metadata)
}

fn inspect_entry(path: &Path, root: &Metadata, result: &mut DiscoveryResult) {
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        problem(result, path, DiscoveryProblemKind::UnsafeName);
        return;
    };
    if name.starts_with('.') {
        return;
    }
    if !is_safe_service_name(name) {
        problem(result, path, DiscoveryProblemKind::UnsafeName);
        return;
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) => {
            problem(result, path, DiscoveryProblemKind::Io(error));
            return;
        }
    };
    if metadata.file_type().is_symlink() {
        problem(result, path, DiscoveryProblemKind::Symlink);
        return;
    }
    if !metadata.is_dir() {
        problem(result, path, DiscoveryProblemKind::WrongType);
        return;
    }
    if metadata.mode() & 0o077 != 0 {
        problem(result, path, DiscoveryProblemKind::UnsafePermissions);
        return;
    }
    if metadata.uid() != root.uid() {
        problem(result, path, DiscoveryProblemKind::OwnerMismatch);
        return;
    }

    let socket = path.join(CONTROL_SOCKET_NAME);
    let socket_metadata = match fs::symlink_metadata(&socket) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            problem(result, &socket, DiscoveryProblemKind::MissingSocket);
            return;
        }
        Err(error) => {
            problem(result, &socket, DiscoveryProblemKind::Io(error));
            return;
        }
    };
    if socket_metadata.file_type().is_symlink() {
        problem(result, &socket, DiscoveryProblemKind::Symlink);
        return;
    }
    if !socket_metadata.file_type().is_socket() {
        problem(result, &socket, DiscoveryProblemKind::WrongType);
        return;
    }
    if socket_metadata.mode() & 0o777 != 0o600 {
        problem(result, &socket, DiscoveryProblemKind::UnsafePermissions);
        return;
    }
    if socket_metadata.uid() != metadata.uid() {
        problem(result, &socket, DiscoveryProblemKind::OwnerMismatch);
        return;
    }
    result.services.insert(
        name.to_owned(),
        RuntimeService {
            name: name.to_owned(),
            directory: path.to_owned(),
            socket,
            owner_uid: metadata.uid(),
        },
    );
}

fn problem(result: &mut DiscoveryResult, path: &Path, kind: DiscoveryProblemKind) {
    result.problems.push(DiscoveryProblem {
        path: path.to_owned(),
        kind,
    });
}

#[cfg(test)]
mod tests {
    use std::{
        error::Error,
        fs, io,
        os::unix::fs::{MetadataExt, PermissionsExt, symlink},
        os::unix::net::UnixListener as StdUnixListener,
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::net::UnixListener;

    use super::{
        CONTROL_SOCKET_NAME, DiscoveryProblemKind, MAX_CONTROL_SOCKET_PATH_BYTES, RuntimeOwner,
        SUPERVISOR_LOCK_NAME, discover, discover_user, discover_with_owner,
        prepare_user_service_directory_at, runtime_root_from_home,
        runtime_root_from_home_for_owner, supervisor_is_active, system_runtime_root,
    };

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    #[test]
    fn platform_system_root_is_absolute_and_native() {
        assert!(system_runtime_root().is_absolute());
        #[cfg(target_os = "linux")]
        assert_eq!(system_runtime_root(), Path::new("/run/immortal"));
        #[cfg(target_os = "freebsd")]
        assert_eq!(system_runtime_root(), Path::new("/var/run/immortal"));
        // The shipped LaunchDaemon installs here, so automatic discovery has to
        // agree or a supported macOS install is invisible to immortalctl.
        #[cfg(target_os = "macos")]
        assert_eq!(system_runtime_root(), Path::new("/var/db/immortal/run"));
    }

    #[test]
    fn prepares_owner_only_user_runtime_root_and_safe_service_path() -> Result<(), Box<dyn Error>> {
        let home = TestDirectory::new()?;
        let root = home.path().join(".immortal");
        let service = prepare_user_service_directory_at(&root, "sleep.30")?;
        assert_eq!(service, root.join("sleep.30"));
        let metadata = fs::symlink_metadata(&root)?;
        assert!(metadata.is_dir());
        assert_eq!(metadata.uid(), nix::unistd::Uid::effective().as_raw());
        assert_eq!(metadata.permissions().mode() & 0o777, 0o700);
        assert_eq!(
            prepare_user_service_directory_at(&root, "sleep.30")?,
            service
        );
        Ok(())
    }

    #[test]
    fn user_runtime_root_canonicalizes_symlinked_home_ancestors() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let actual_parent = directory.path().join("actual");
        let actual_home = actual_parent.join("user");
        fs::create_dir(&actual_parent)?;
        fs::create_dir(&actual_home)?;
        fs::set_permissions(&actual_home, fs::Permissions::from_mode(0o700))?;
        let alias_parent = directory.path().join("home");
        symlink(&actual_parent, &alias_parent)?;

        assert_eq!(
            runtime_root_from_home(&alias_parent.join("user"))?,
            actual_home.join(".immortal")
        );
        Ok(())
    }

    #[test]
    fn user_runtime_root_rejects_untrusted_home_directory() -> Result<(), Box<dyn Error>> {
        let directory = TestDirectory::new()?;
        let owner = fs::symlink_metadata(directory.path())?.uid();
        let mismatched = owner.checked_add(1).unwrap_or(owner.saturating_sub(1));
        let owner_error = runtime_root_from_home_for_owner(directory.path(), mismatched)
            .err()
            .ok_or("home owned by another UID was accepted")?;
        assert_eq!(owner_error.kind(), io::ErrorKind::PermissionDenied);

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o777))?;
        let error = runtime_root_from_home(directory.path())
            .err()
            .ok_or("world-writable home directory was accepted")?;
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        Ok(())
    }

    #[test]
    fn user_runtime_preparation_rejects_unsafe_names_and_roots() -> Result<(), Box<dyn Error>> {
        let home = TestDirectory::new()?;
        let root = home.path().join(".immortal");
        for name in ["", ".", "..", ".hidden", "bad/name", "bad name"] {
            let error = prepare_user_service_directory_at(&root, name)
                .err()
                .ok_or("unsafe service name was accepted")?;
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        assert!(!root.exists());
        let long_name = "a".repeat(255);
        let path_error = prepare_user_service_directory_at(&root, &long_name)
            .err()
            .ok_or("overlong control socket path was accepted")?;
        assert_eq!(path_error.kind(), io::ErrorKind::InvalidInput);
        assert!(
            path_error
                .to_string()
                .contains(&MAX_CONTROL_SOCKET_PATH_BYTES.to_string())
        );
        assert!(!root.exists());

        fs::create_dir(&root)?;
        fs::set_permissions(&root, fs::Permissions::from_mode(0o755))?;
        let mode_error = prepare_user_service_directory_at(&root, "api")
            .err()
            .ok_or("unsafe user runtime mode was accepted")?;
        assert_eq!(mode_error.kind(), io::ErrorKind::PermissionDenied);
        fs::remove_dir(&root)?;

        let target = home.path().join("runtime-target");
        fs::create_dir(&target)?;
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700))?;
        symlink(&target, &root)?;
        let symlink_error = prepare_user_service_directory_at(&root, "api")
            .err()
            .ok_or("symlinked user runtime root was accepted")?;
        assert_eq!(symlink_error.kind(), io::ErrorKind::InvalidInput);
        Ok(())
    }

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = Path::new("/tmp").join(format!(
                "immortal-runtime-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
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

    #[tokio::test(flavor = "current_thread")]
    async fn discovers_only_permission_safe_owned_sockets() -> Result<(), Box<dyn Error>> {
        let root = TestDirectory::new()?;
        let service = root.path().join("api");
        fs::create_dir(&service)?;
        fs::set_permissions(&service, fs::Permissions::from_mode(0o700))?;
        let socket = service.join("immortal.sock");
        let listener = UnixListener::bind(&socket)?;
        fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;

        let result = discover(root.path())?;
        let discovered = result.services.get("api").ok_or("api not discovered")?;
        assert_eq!(discovered.socket, socket);
        assert!(result.problems.is_empty());
        drop(listener);
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn isolates_symlinks_wrong_modes_and_unrelated_entries() -> Result<(), Box<dyn Error>> {
        let root = TestDirectory::new()?;
        fs::write(root.path().join("notes"), b"unrelated")?;
        let open = root.path().join("open");
        fs::create_dir(&open)?;
        fs::set_permissions(&open, fs::Permissions::from_mode(0o755))?;
        let linked = root.path().join("linked");
        symlink(&open, &linked)?;

        let result = discover(root.path())?;
        assert!(result.services.is_empty());
        assert!(
            result
                .problems
                .iter()
                .any(|problem| matches!(problem.kind, DiscoveryProblemKind::WrongType))
        );
        assert!(
            result
                .problems
                .iter()
                .any(|problem| matches!(problem.kind, DiscoveryProblemKind::UnsafePermissions))
        );
        assert!(
            result
                .problems
                .iter()
                .any(|problem| matches!(problem.kind, DiscoveryProblemKind::Symlink))
        );
        Ok(())
    }

    #[test]
    fn rejects_relative_and_world_writable_roots() -> Result<(), Box<dyn Error>> {
        assert!(discover(Path::new("relative")).is_err());
        let root = TestDirectory::new()?;
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o777))?;
        assert!(discover(root.path()).is_err());
        Ok(())
    }

    #[test]
    fn user_discovery_accepts_only_the_effective_users_root() -> Result<(), Box<dyn Error>> {
        let root = TestDirectory::new()?;
        let result = discover_user(root.path())?;
        assert!(result.services.is_empty());
        assert!(result.problems.is_empty());
        let owner = fs::symlink_metadata(root.path())?.uid();
        let mismatched = owner.checked_add(1).unwrap_or(owner.saturating_sub(1));
        assert!(discover_with_owner(root.path(), Some(mismatched)).is_err());
        Ok(())
    }

    #[test]
    fn runtime_owner_locks_before_removing_an_owned_stale_socket() -> Result<(), Box<dyn Error>> {
        let root = TestDirectory::new()?;
        let service = root.path().join("api");
        let owner = RuntimeOwner::acquire(&service)?;
        assert!(supervisor_is_active(&service)?);
        assert_eq!(owner.directory(), service);
        assert_eq!(owner.socket(), service.join(CONTROL_SOCKET_NAME));
        assert_eq!(
            fs::symlink_metadata(&service)?.permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::symlink_metadata(service.join(SUPERVISOR_LOCK_NAME))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let stale = StdUnixListener::bind(owner.socket())?;
        fs::set_permissions(owner.socket(), fs::Permissions::from_mode(0o600))?;
        drop(stale);
        let locked_error = RuntimeOwner::acquire(&service)
            .err()
            .ok_or("a second runtime owner unexpectedly acquired the supervisor lock")?;
        assert_eq!(locked_error.kind(), io::ErrorKind::AlreadyExists);
        assert!(owner.socket().exists());

        drop(owner);
        assert!(!supervisor_is_active(&service)?);
        let replacement = RuntimeOwner::acquire(&service)?;
        assert!(!replacement.socket().exists());
        Ok(())
    }

    #[test]
    fn runtime_owner_refuses_unsafe_lock_and_socket_entries() -> Result<(), Box<dyn Error>> {
        let root = TestDirectory::new()?;
        let service = root.path().join("worker");
        fs::create_dir(&service)?;
        fs::set_permissions(&service, fs::Permissions::from_mode(0o700))?;
        let target = root.path().join("target");
        fs::write(&target, b"do not follow")?;
        symlink(&target, service.join(SUPERVISOR_LOCK_NAME))?;
        assert!(RuntimeOwner::acquire(&service).is_err());
        fs::remove_file(service.join(SUPERVISOR_LOCK_NAME))?;

        fs::write(service.join(CONTROL_SOCKET_NAME), b"not a socket")?;
        assert!(RuntimeOwner::acquire(&service).is_err());
        assert!(service.join(CONTROL_SOCKET_NAME).is_file());
        Ok(())
    }

    #[test]
    fn runtime_owner_rejects_nonportable_socket_path_before_creation() -> Result<(), Box<dyn Error>>
    {
        let root = TestDirectory::new()?;
        let service = root.path().join("a".repeat(255));
        let error = RuntimeOwner::acquire(&service)
            .err()
            .ok_or("non-portable control socket path was accepted")?;
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(!service.exists());
        Ok(())
    }
}
