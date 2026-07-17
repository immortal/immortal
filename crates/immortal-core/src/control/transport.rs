//! Asynchronous bounded frame I/O for the local control protocol.
//!
//! Transport first reads the fixed header with an idle deadline, derives the
//! declared payload length, rejects oversized frames before allocating the
//! payload, then delegates complete-frame validation to `message`. Writes encode
//! a full request or response before a single timed write/flush. Peer
//! authorization is deliberately not handled here; the Unix server authenticates
//! peers before calling these helpers.

#[cfg(unix)]
use std::path::Path;
use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    time::Duration,
};

#[cfg(unix)]
use tokio::net::UnixStream;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    time::timeout,
};

use super::{
    CONTROL_IO_TIMEOUT, MAX_FRAME_BYTES, ProtocolError, Request, Response, wire::HEADER_BYTES,
};

/// Failure while transferring one bounded frame.
#[derive(Debug)]
pub enum TransportError {
    /// Asynchronous stream I/O failed.
    Io(io::Error),
    /// Complete frame violated the control protocol.
    Protocol(ProtocolError),
    /// Peer did not complete the operation before the idle deadline.
    Timeout,
}

impl Display for TransportError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "control transport I/O failed: {error}"),
            Self::Protocol(error) => Display::fmt(error, formatter),
            Self::Timeout => formatter.write_str("control transport deadline exceeded"),
        }
    }
}

impl Error for TransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Protocol(error) => Some(error),
            Self::Timeout => None,
        }
    }
}

impl From<io::Error> for TransportError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<ProtocolError> for TransportError {
    fn from(error: ProtocolError) -> Self {
        Self::Protocol(error)
    }
}

/// Read exactly one request using the default idle deadline.
///
/// # Errors
///
/// Returns an error for I/O failure, timeout, oversized input, or an invalid
/// request frame.
pub async fn read_request<R>(reader: &mut R) -> Result<Request, TransportError>
where
    R: AsyncRead + Unpin,
{
    read_request_with_timeout(reader, CONTROL_IO_TIMEOUT).await
}

/// Read exactly one request using an explicit idle deadline.
///
/// # Errors
///
/// Returns an error for I/O failure, timeout, oversized input, or an invalid
/// request frame.
pub async fn read_request_with_timeout<R>(
    reader: &mut R,
    idle_timeout: Duration,
) -> Result<Request, TransportError>
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame_with_timeout(reader, idle_timeout).await?;
    Request::decode(&frame).map_err(TransportError::Protocol)
}

/// Write exactly one request using the default idle deadline.
///
/// # Errors
///
/// Returns an error for request encoding, I/O failure, or timeout.
pub async fn write_request<W>(writer: &mut W, request: &Request) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let frame = request.encode()?;
    write_frame_with_timeout(writer, &frame, CONTROL_IO_TIMEOUT).await
}

/// Read exactly one response using the default idle deadline.
///
/// # Errors
///
/// Returns an error for I/O failure, timeout, oversized input, or an invalid
/// response frame.
pub async fn read_response<R>(reader: &mut R) -> Result<Response, TransportError>
where
    R: AsyncRead + Unpin,
{
    let frame = read_frame_with_timeout(reader, CONTROL_IO_TIMEOUT).await?;
    Response::decode(&frame).map_err(TransportError::Protocol)
}

/// Write exactly one response using the default idle deadline.
///
/// # Errors
///
/// Returns an error for response encoding, I/O failure, or timeout.
pub async fn write_response<W>(writer: &mut W, response: &Response) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    let frame = response.encode()?;
    write_frame_with_timeout(writer, &frame, CONTROL_IO_TIMEOUT).await
}

/// Connect to one local supervisor and exchange exactly one bounded request.
///
/// # Errors
///
/// Returns a timeout, Unix-socket I/O, request encoding, or response protocol
/// failure. Peer authorization remains enforced by the supervisor.
#[cfg(unix)]
pub async fn exchange(path: &Path, request: &Request) -> Result<Response, TransportError> {
    let mut stream = timeout(CONTROL_IO_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| TransportError::Timeout)??;
    write_request(&mut stream, request).await?;
    read_response(&mut stream).await
}

async fn read_frame_with_timeout<R>(
    reader: &mut R,
    idle_timeout: Duration,
) -> Result<Vec<u8>, TransportError>
where
    R: AsyncRead + Unpin,
{
    timeout(idle_timeout, read_frame(reader))
        .await
        .map_err(|_| TransportError::Timeout)?
}

async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, TransportError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; HEADER_BYTES];
    reader.read_exact(&mut header).await?;
    let length_bytes: [u8; 2] = header
        .get(HEADER_BYTES - 2..)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let payload_length = usize::from(u16::from_be_bytes(length_bytes));
    let frame_length = HEADER_BYTES
        .checked_add(payload_length)
        .ok_or(ProtocolError::FrameTooLarge(usize::MAX))?;
    if frame_length > MAX_FRAME_BYTES {
        return Err(ProtocolError::FrameTooLarge(frame_length).into());
    }
    let mut payload = vec![0_u8; payload_length];
    reader.read_exact(&mut payload).await?;
    let mut frame = Vec::with_capacity(frame_length);
    frame.extend_from_slice(&header);
    frame.extend_from_slice(&payload);
    Ok(frame)
}

async fn write_frame_with_timeout<W>(
    writer: &mut W,
    frame: &[u8],
    idle_timeout: Duration,
) -> Result<(), TransportError>
where
    W: AsyncWrite + Unpin,
{
    timeout(idle_timeout, async {
        writer.write_all(frame).await?;
        writer.flush().await
    })
    .await
    .map_err(|_| TransportError::Timeout)??;
    Ok(())
}
