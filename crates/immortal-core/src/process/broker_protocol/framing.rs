//! Bounded frame envelope shared by every broker request and event.
//!
//! Every message on the wire begins with a fixed ten-byte header: a four-byte
//! magic value, a one-byte protocol version, a one-byte message kind, and a
//! four-byte big-endian payload length. [`declared_frame_length`] lets the
//! Tokio-side reader learn the exact frame size from the header alone, before
//! it has read the payload, so it can bound one `read_exact` call instead of
//! trusting an attacker- or bug-controlled length. `Cursor` then walks the
//! decoded payload one bounded field at a time and fails closed on
//! truncation or trailing bytes instead of panicking or under-reading.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;

use super::error::BrokerProtocolError;

const MAGIC: [u8; 4] = *b"IMBR";
pub(in crate::process) const HEADER_BYTES: usize = 10;
const VERSION: u8 = 6;

pub(in crate::process) const MAX_FRAME_BYTES: usize = 1024 * 1024;

pub(super) const MAX_FIELD_BYTES: usize = 256 * 1024;

/// Bounded cursor over one decoded frame payload.
///
/// Every accessor advances the position and returns a typed error instead of
/// panicking when the payload is shorter than the field being read.
pub(super) struct Cursor<'frame> {
    frame: &'frame [u8],
    position: usize,
}

impl<'frame> Cursor<'frame> {
    pub(super) const fn new(frame: &'frame [u8]) -> Self {
        Self { frame, position: 0 }
    }

    pub(super) fn take<const N: usize>(&mut self) -> Result<[u8; N], BrokerProtocolError> {
        let bytes = self.bytes(N)?;
        bytes.try_into().map_err(|_| BrokerProtocolError::Truncated)
    }

    pub(super) fn byte(&mut self) -> Result<u8, BrokerProtocolError> {
        Ok(self.take::<1>()?[0])
    }

    pub(super) fn bytes(&mut self, length: usize) -> Result<&'frame [u8], BrokerProtocolError> {
        let end = self
            .position
            .checked_add(length)
            .ok_or(BrokerProtocolError::Truncated)?;
        let bytes = self
            .frame
            .get(self.position..end)
            .ok_or(BrokerProtocolError::Truncated)?;
        self.position = end;
        Ok(bytes)
    }

    pub(super) fn os_string(&mut self) -> Result<OsString, BrokerProtocolError> {
        let length = usize::try_from(u32::from_be_bytes(self.take::<4>()?))
            .map_err(|_| BrokerProtocolError::FieldTooLarge(usize::MAX))?;
        if length > MAX_FIELD_BYTES {
            return Err(BrokerProtocolError::FieldTooLarge(length));
        }
        Ok(OsString::from_vec(self.bytes(length)?.to_vec()))
    }

    pub(super) const fn remaining(&self) -> usize {
        self.frame.len().saturating_sub(self.position)
    }

    pub(super) fn finish(&self) -> Result<(), BrokerProtocolError> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(BrokerProtocolError::TrailingBytes)
        }
    }
}

pub(super) fn decode_boolean(cursor: &mut Cursor<'_>) -> Result<bool, BrokerProtocolError> {
    match cursor.byte()? {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(BrokerProtocolError::InvalidBoolean),
    }
}

pub(super) fn encode_frame(kind: u8, payload: &[u8]) -> Result<Vec<u8>, BrokerProtocolError> {
    let frame_length = HEADER_BYTES.saturating_add(payload.len());
    if frame_length > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame_length));
    }
    let payload_length = u32::try_from(payload.len())
        .map_err(|_| BrokerProtocolError::FrameTooLarge(frame_length))?;
    let mut frame = Vec::with_capacity(frame_length);
    frame.extend_from_slice(&MAGIC);
    frame.push(VERSION);
    frame.push(kind);
    frame.extend_from_slice(&payload_length.to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub(super) fn decode_frame(frame: &[u8]) -> Result<(u8, &[u8]), BrokerProtocolError> {
    if frame.len() > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame.len()));
    }
    let mut cursor = Cursor::new(frame);
    if cursor.take::<4>()? != MAGIC {
        return Err(BrokerProtocolError::InvalidMagic);
    }
    let version = cursor.byte()?;
    if version != VERSION {
        return Err(BrokerProtocolError::UnsupportedVersion(version));
    }
    let kind = cursor.byte()?;
    let payload_length = usize::try_from(u32::from_be_bytes(cursor.take::<4>()?))
        .map_err(|_| BrokerProtocolError::LengthMismatch)?;
    if cursor.remaining() != payload_length {
        return Err(BrokerProtocolError::LengthMismatch);
    }
    Ok((kind, cursor.bytes(payload_length)?))
}

/// Learn the exact frame length declared by a just-read header.
///
/// # Errors
///
/// Returns a bounded protocol error when the magic, version, or declared
/// length is invalid, or when the declared frame would exceed
/// `MAX_FRAME_BYTES`.
pub(in crate::process) fn declared_frame_length(
    header: &[u8; HEADER_BYTES],
) -> Result<usize, BrokerProtocolError> {
    if header.get(..4) != Some(MAGIC.as_slice()) {
        return Err(BrokerProtocolError::InvalidMagic);
    }
    let version = header
        .get(4)
        .copied()
        .ok_or(BrokerProtocolError::Truncated)?;
    if version != VERSION {
        return Err(BrokerProtocolError::UnsupportedVersion(version));
    }
    let length_bytes: [u8; 4] = header
        .get(6..10)
        .ok_or(BrokerProtocolError::Truncated)?
        .try_into()
        .map_err(|_| BrokerProtocolError::Truncated)?;
    let payload_length = usize::try_from(u32::from_be_bytes(length_bytes))
        .map_err(|_| BrokerProtocolError::LengthMismatch)?;
    let frame_length = HEADER_BYTES.saturating_add(payload_length);
    if frame_length > MAX_FRAME_BYTES {
        return Err(BrokerProtocolError::FrameTooLarge(frame_length));
    }
    Ok(frame_length)
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::io;

    use crate::process::ProcessSignal;
    use crate::supervisor::Generation;

    use super::super::error::BrokerProtocolError;
    use super::super::request::{BrokerRequest, BrokerSignalTarget};
    use super::MAX_FRAME_BYTES;

    #[test]
    fn malformed_and_oversized_frames_fail_closed() -> Result<(), Box<dyn Error>> {
        let generation = Generation::new(1).ok_or("invalid test generation")?;
        let request = BrokerRequest::Signal {
            generation,
            target: BrokerSignalTarget::Process,
            signal: ProcessSignal::Terminate,
        };
        let frame = request.encode()?;
        for length in 0..frame.len() {
            let truncated = frame
                .get(..length)
                .ok_or_else(|| io::Error::other("invalid truncation length"))?;
            assert!(BrokerRequest::decode(truncated).is_err());
        }
        let mut trailing = frame.clone();
        trailing.push(0);
        assert_eq!(
            BrokerRequest::decode(&trailing),
            Err(BrokerProtocolError::LengthMismatch)
        );
        assert_eq!(
            BrokerRequest::decode(&vec![0; MAX_FRAME_BYTES + 1]),
            Err(BrokerProtocolError::FrameTooLarge(MAX_FRAME_BYTES + 1))
        );
        Ok(())
    }
}
