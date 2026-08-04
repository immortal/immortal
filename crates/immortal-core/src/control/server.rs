//! Unix-socket control server and peer-authorization boundary.
//!
//! Binding validates that the socket path is absolute, canonical, owned with
//! its runtime directory, not pre-existing, and mode-restricted to the owner.
//! Accept flow is bounded by a semaphore and idle deadline: acquire a client
//! slot, accept one stream, read kernel peer credentials, authorize root or the
//! socket owner, then hand an `AuthorizedConnection` to a per-client task. The
//! supervisor event loop remains the only lifecycle-state owner; connection
//! tasks decode one request, forward it over a bounded channel, and write the
//! eventual response. Listener drop removes only the socket inode it created.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    fs, io,
    os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use tokio::{
    net::{UnixListener, UnixStream},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot, watch},
    task::JoinSet,
    time::{sleep, timeout},
};

use super::{
    CONTROL_IO_TIMEOUT, Request, Response, ResponseCode, TransportError,
    transport::{read_request, write_response},
};

/// Authenticated peer identity obtained from the Unix socket.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PeerCredentials {
    /// Effective user ID of the connecting process.
    pub uid: u32,
    /// Effective group ID of the connecting process.
    pub gid: u32,
    /// Process ID when the operating system exposes it.
    pub pid: Option<i32>,
}

/// An authorized connection which occupies one bounded server slot.
#[cfg(unix)]
#[derive(Debug)]
pub struct AuthorizedConnection {
    stream: UnixStream,
    peer: PeerCredentials,
    _permit: OwnedSemaphorePermit,
}

#[cfg(unix)]
impl AuthorizedConnection {
    /// Credentials authenticated when this connection was accepted.
    #[must_use]
    pub const fn peer(&self) -> PeerCredentials {
        self.peer
    }

    /// Borrow the stream for bounded request and response transfer.
    pub const fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.stream
    }

    /// Consume the authorization guard and return its stream.
    #[must_use]
    pub fn into_stream(self) -> UnixStream {
        self.stream
    }
}

/// Authenticated request forwarded to the single-owner supervisor event loop.
#[cfg(unix)]
#[derive(Debug)]
pub struct ControlCommand {
    request: Request,
    peer: PeerCredentials,
    response: oneshot::Sender<Response>,
}

#[cfg(unix)]
impl ControlCommand {
    /// Validated bounded request received from the peer.
    #[must_use]
    pub const fn request(&self) -> &Request {
        &self.request
    }

    /// Credentials authenticated before reading the request.
    #[must_use]
    pub const fn peer(&self) -> PeerCredentials {
        self.peer
    }

    /// Complete this request after lifecycle work and status publication.
    ///
    /// # Errors
    ///
    /// Returns the boxed response when the client disconnected before completion.
    pub fn respond(self, response: Response) -> Result<(), Box<Response>> {
        self.response.send(response).map_err(Box::new)
    }
}

/// Run the authenticated socket side of the control server.
///
/// Backoff applied after an accept failure caused by exhausted descriptors or
/// kernel buffers.
///
/// Those conditions persist until an unrelated descriptor is released, so
/// retrying immediately would spin the accept loop at full speed while the
/// supervisor still has to service process events, timers, and signals. The
/// delay is deliberately short: the listener stays bound throughout, so it only
/// paces retries and never delays an accept that could have succeeded.
#[cfg(unix)]
const ACCEPT_EXHAUSTION_BACKOFF: Duration = Duration::from_millis(100);

/// Whether an accept failure leaves the listener usable.
///
/// A connection aborted or reset between `accept` and the credential read is
/// attributable to the peer, and an interrupted or would-block accept is
/// attributable to scheduling; neither says anything about the listener. So is
/// descriptor or buffer exhaustion, which is a whole-process resource condition
/// that resolves on its own. Anything else is treated as an unusable listener,
/// because continuing would hide a permanently broken control endpoint behind
/// an infinite loop.
///
/// `EMFILE`, `ENFILE`, and `ENOBUFS` have no stable [`io::ErrorKind`] across the
/// supported platforms and Rust versions, so they are matched on the raw errno.
#[cfg(unix)]
pub(super) fn accept_error_is_transient(error: &io::Error) -> bool {
    if matches!(
        error.kind(),
        io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::Interrupted
            | io::ErrorKind::WouldBlock
    ) {
        return true;
    }
    accept_error_is_exhaustion(error)
}

/// Whether an accept failure is a transient resource shortage worth pacing.
#[cfg(unix)]
pub(super) fn accept_error_is_exhaustion(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
    )
}

/// Each accepted client is bounded by [`ControlListener`], frame deadlines, and
/// the supplied bounded command channel. The receiving supervisor event loop
/// remains the only owner of lifecycle state. Malformed/disconnected clients
/// are isolated to their connection task. Shutdown aborts and joins every
/// remaining client task before returning.
///
/// Transient accept failures (peer aborts, interruptions, and descriptor or
/// buffer exhaustion) are absorbed so a single unlucky connection cannot stop
/// the supervisor.
///
/// # Errors
///
/// Returns a fatal listener failure or a closed command receiver.
#[cfg(unix)]
pub async fn run_control_server(
    listener: Arc<ControlListener>,
    commands: mpsc::Sender<ControlCommand>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), AcceptError> {
    let mut tasks = JoinSet::new();
    if *shutdown.borrow() {
        return Ok(());
    }
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
            () = commands.closed() => return Err(AcceptError::ShuttingDown),
            accepted = listener.accept() => match accepted {
                Ok(connection) => {
                    let sender = commands.clone();
                    tasks.spawn(async move {
                        let _isolated_error = serve_control_connection(connection, sender).await;
                    });
                }
                Err(
                    AcceptError::Timeout
                    | AcceptError::PeerCredentials(_)
                    | AcceptError::PermissionDenied { .. },
                ) => {}
                Err(AcceptError::Io(error)) if accept_error_is_transient(&error) => {
                    if accept_error_is_exhaustion(&error) {
                        sleep(ACCEPT_EXHAUSTION_BACKOFF).await;
                    }
                }
                Err(error) => return Err(error),
            },
            Some(_completed) = tasks.join_next(), if !tasks.is_empty() => {}
        }
    }
    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    Ok(())
}

#[cfg(unix)]
async fn serve_control_connection(
    mut connection: AuthorizedConnection,
    commands: mpsc::Sender<ControlCommand>,
) -> Result<(), TransportError> {
    let request = read_request(connection.stream_mut()).await?;
    let peer = connection.peer();
    let (response, receiver) = oneshot::channel();
    commands
        .send(ControlCommand {
            request,
            peer,
            response,
        })
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::BrokenPipe,
                "supervisor control command receiver closed",
            )
        })?;
    let response = receiver.await.map_err(|_| {
        io::Error::new(
            io::ErrorKind::BrokenPipe,
            "supervisor dropped a control response",
        )
    })?;
    write_response(connection.stream_mut(), &response).await
}

/// Failure while accepting and authenticating a control connection.
#[cfg(unix)]
#[derive(Debug)]
pub enum AcceptError {
    /// Listener operation failed.
    Io(io::Error),
    /// The accepted peer disconnected before its credentials were available.
    PeerCredentials(io::Error),
    /// No client slot or connection arrived before the deadline.
    Timeout,
    /// Semaphore was closed during shutdown.
    ShuttingDown,
    /// Peer is neither root nor the supervisor owner.
    PermissionDenied {
        /// Effective UID rejected by the server.
        uid: u32,
    },
}

#[cfg(unix)]
impl Display for AcceptError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "unable to accept control connection: {error}"),
            Self::PeerCredentials(error) => {
                write!(formatter, "unable to authenticate control peer: {error}")
            }
            Self::Timeout => formatter.write_str("control accept deadline exceeded"),
            Self::ShuttingDown => formatter.write_str("control listener is shutting down"),
            Self::PermissionDenied { uid } => {
                write!(formatter, "control peer UID {uid} is not authorized")
            }
        }
    }
}

#[cfg(unix)]
impl Error for AcceptError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) | Self::PeerCredentials(error) => Some(error),
            Self::Timeout | Self::ShuttingDown | Self::PermissionDenied { .. } => None,
        }
    }
}

#[cfg(unix)]
impl From<io::Error> for AcceptError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug)]
struct SocketIdentity {
    device: u64,
    inode: u64,
}

/// Owned, permission-restricted, authenticated local control listener.
#[cfg(unix)]
#[derive(Debug)]
pub struct ControlListener {
    listener: UnixListener,
    path: PathBuf,
    identity: SocketIdentity,
    owner_uid: u32,
    clients: Arc<Semaphore>,
}

#[cfg(unix)]
impl ControlListener {
    /// Bind a new control socket without deleting or replacing an existing path.
    ///
    /// The path must be absolute, its parent must be canonical, owned by the
    /// effective account creating the socket, and not group/world writable.
    /// The resulting socket mode is `0600`.
    ///
    /// # Errors
    ///
    /// Returns an error for unsafe paths, an existing entry, invalid client
    /// limits, binding failure, or permission/metadata failure.
    pub fn bind(path: &Path, max_clients: usize) -> io::Result<Self> {
        if max_clients == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "maximum control clients must be greater than zero",
            ));
        }
        validate_socket_parent(path)?;
        match fs::symlink_metadata(path) {
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "control socket path already exists",
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }

        let listener = UnixListener::bind(path)?;
        if let Err(error) = fs::set_permissions(path, fs::Permissions::from_mode(0o600)) {
            cleanup_created_socket(path);
            return Err(error);
        }
        let metadata = match fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) => {
                cleanup_created_socket(path);
                return Err(error);
            }
        };
        if !metadata.file_type().is_socket() {
            cleanup_created_socket(path);
            return Err(io::Error::other("new control socket path is not a socket"));
        }
        let Some(parent) = path.parent() else {
            cleanup_created_socket(path);
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "control socket has no parent",
            ));
        };
        let parent_metadata = match fs::symlink_metadata(parent) {
            Ok(metadata) => metadata,
            Err(error) => {
                cleanup_created_socket(path);
                return Err(error);
            }
        };
        if metadata.uid() != parent_metadata.uid() {
            cleanup_created_socket(path);
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "control socket and runtime directory owners differ",
            ));
        }

        Ok(Self {
            listener,
            path: path.to_owned(),
            identity: SocketIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            owner_uid: metadata.uid(),
            clients: Arc::new(Semaphore::new(max_clients)),
        })
    }

    /// Socket path owned by this listener.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Effective UID authorized in addition to root.
    #[must_use]
    pub const fn owner_uid(&self) -> u32 {
        self.owner_uid
    }

    /// Pretend the socket belongs to another user so rejection can be tested.
    #[cfg(test)]
    pub(in crate::control) const fn set_owner_uid_for_test(&mut self, uid: u32) {
        self.owner_uid = uid;
    }

    /// Accept and authenticate a connection with the default idle deadline.
    ///
    /// # Errors
    ///
    /// Returns an error for timeout, shutdown, listener/credential failure, or
    /// an unauthorized peer.
    pub async fn accept(&self) -> Result<AuthorizedConnection, AcceptError> {
        self.accept_with_timeout(CONTROL_IO_TIMEOUT).await
    }

    /// Accept and authenticate a connection with an explicit idle deadline.
    ///
    /// # Errors
    ///
    /// Returns an error for timeout, shutdown, listener/credential failure, or
    /// an unauthorized peer.
    pub async fn accept_with_timeout(
        &self,
        idle_timeout: Duration,
    ) -> Result<AuthorizedConnection, AcceptError> {
        let permit = timeout(idle_timeout, Arc::clone(&self.clients).acquire_owned())
            .await
            .map_err(|_| AcceptError::Timeout)?
            .map_err(|_| AcceptError::ShuttingDown)?;
        let (stream, _) = timeout(idle_timeout, self.listener.accept())
            .await
            .map_err(|_| AcceptError::Timeout)??;
        let credentials = stream.peer_cred().map_err(AcceptError::PeerCredentials)?;
        let peer = PeerCredentials {
            uid: credentials.uid(),
            gid: credentials.gid(),
            pid: credentials.pid(),
        };
        if !peer_is_authorized(peer.uid, self.owner_uid) {
            reject_unauthorized_peer(stream, permit);
            return Err(AcceptError::PermissionDenied { uid: peer.uid });
        }
        Ok(AuthorizedConnection {
            stream,
            peer,
            _permit: permit,
        })
    }
}

#[cfg(unix)]
impl Drop for ControlListener {
    fn drop(&mut self) {
        let Ok(metadata) = fs::symlink_metadata(&self.path) else {
            return;
        };
        if metadata.file_type().is_socket()
            && metadata.dev() == self.identity.device
            && metadata.ino() == self.identity.inode
        {
            let _ignored = fs::remove_file(&self.path);
        }
    }
}

#[cfg(unix)]
fn validate_socket_parent(path: &Path) -> io::Result<()> {
    if !path.is_absolute() || path.file_name().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket path must be an absolute file path",
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "control socket has no parent")
    })?;
    let canonical = fs::canonicalize(parent)?;
    if canonical != parent {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket parent must be canonical and contain no symlink",
        ));
    }
    let metadata = fs::symlink_metadata(parent)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "control socket parent is not a real directory",
        ));
    }
    if metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "control socket parent must not be group or world writable",
        ));
    }
    Ok(())
}

#[cfg(unix)]
/// Decide whether a connected peer may act on this supervisor.
///
/// This is the only authorization predicate in the control protocol. Root and
/// the uid which owns the socket are accepted; every other peer is refused.
/// The owner uid is read from the socket the listener created, never from the
/// request, so a peer cannot assert its own identity. Authorization is
/// deliberately independent of the socket file mode, which is defense in depth
/// rather than the check itself.
pub(in crate::control) const fn peer_is_authorized(peer_uid: u32, owner_uid: u32) -> bool {
    peer_uid == 0 || peer_uid == owner_uid
}

/// Tell an unauthorized peer why it was refused, then close the connection.
///
/// Dropping the stream silently left the client with an unexpected end of
/// stream, which it can only classify as a transport fault even though the
/// refusal is definite. The rejection is written from a spawned task holding
/// the client permit it was already granted, so a peer which never reads
/// cannot stall the accept loop and concurrent rejections stay bounded by the
/// same client limit as real connections. Failures are deliberately ignored:
/// the peer is unauthorized either way.
#[cfg(unix)]
#[cfg(unix)]
fn reject_unauthorized_peer(mut stream: UnixStream, permit: OwnedSemaphorePermit) {
    tokio::spawn(async move {
        let response = Response {
            code: ResponseCode::PermissionDenied,
            generation: None,
            message: "control peer is not authorized".to_owned(),
            status: None,
        };
        let _ignored = write_response(&mut stream, &response).await;
        drop(permit);
    });
}

#[cfg(unix)]
fn cleanup_created_socket(path: &Path) {
    let _ignored = fs::remove_file(path);
}
