//! Safe runtime-directory discovery shared by control and reconciliation.

use std::{
    collections::BTreeMap,
    error::Error,
    fmt::{self, Display, Formatter},
    fs::{self, Metadata},
    io,
    os::unix::fs::{FileTypeExt, MetadataExt},
    path::{Path, PathBuf},
};

/// Control socket filename inside each service runtime directory.
pub const CONTROL_SOCKET_NAME: &str = "immortal.sock";
/// Maximum safely discoverable services in one runtime root.
pub const MAX_RUNTIME_SERVICES: usize = 4096;

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
    let root_metadata = validate_root(root).map_err(RuntimeRootError)?;
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
    if !safe_service_name(name) {
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

fn safe_service_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
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
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::{Path, PathBuf},
        sync::atomic::{AtomicU64, Ordering},
    };

    use tokio::net::UnixListener;

    use super::{DiscoveryProblemKind, discover};

    static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

    struct TestDirectory(PathBuf);

    impl TestDirectory {
        fn new() -> Result<Self, Box<dyn Error>> {
            let sequence = NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "immortal-runtime-{}-{sequence}",
                std::process::id()
            ));
            fs::create_dir(&path)?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
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
}
