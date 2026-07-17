//! Bounded frame read/write pairing for every broker request and event.
//!
//! Each function pairs one wire type's `encode`/`decode` with the shared
//! bounded frame writer/reader so callers never see a partially written or
//! partially read frame: [`write_frame`]/[`read_frame`] enforce the declared
//! length against [`MAX_FRAME_BYTES`] before any payload is trusted.

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use super::error::ProcessBrokerError;
use super::{
    BrokerEvent, BrokerProtocolError, BrokerRequest, HEADER_BYTES, MAX_FRAME_BYTES,
    declared_frame_length,
};

pub(super) async fn write_request<W>(
    writer: &mut W,
    request: &BrokerRequest,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, &request.encode()?).await
}

pub(super) async fn read_request<R>(reader: &mut R) -> Result<BrokerRequest, ProcessBrokerError>
where
    R: AsyncRead + Unpin,
{
    BrokerRequest::decode(&read_frame(reader).await?).map_err(Into::into)
}

pub(super) async fn write_event<W>(
    writer: &mut W,
    event: &BrokerEvent,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    write_frame(writer, &event.encode()?).await
}

pub(super) async fn read_event<R>(reader: &mut R) -> Result<BrokerEvent, ProcessBrokerError>
where
    R: AsyncRead + Unpin,
{
    BrokerEvent::decode(&read_frame(reader).await?).map_err(Into::into)
}

async fn write_frame<W>(writer: &mut W, frame: &[u8]) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    writer.write_all(frame).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R>(reader: &mut R) -> Result<Vec<u8>, ProcessBrokerError>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0; HEADER_BYTES];
    let _ = reader.read_exact(&mut header).await?;
    let frame_length = declared_frame_length(&header)?;
    let payload_length = frame_length.saturating_sub(HEADER_BYTES);
    if frame_length > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame_length).into());
    }
    let mut frame = Vec::with_capacity(frame_length);
    frame.extend_from_slice(&header);
    frame.resize(frame_length, 0);
    let payload = frame
        .get_mut(HEADER_BYTES..)
        .ok_or(BrokerProtocolError::Truncated)?;
    if payload.len() != payload_length {
        return Err(BrokerProtocolError::LengthMismatch.into());
    }
    let _ = reader.read_exact(payload).await?;
    Ok(frame)
}
