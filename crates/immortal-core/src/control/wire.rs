//! Shared frame envelope constants and bounded decode cursor.
//!
//! Control frames begin with fixed magic, version, operation/result fields, and
//! a two-byte payload length at the end of the 18-byte header. Message codecs
//! build or validate the full frame, while transport reads only the header
//! length before allocating payload storage. `Cursor` advances through decoded
//! bytes with checked splits so malformed, truncated, or trailing input becomes
//! a typed protocol error rather than an unchecked index or partial parse.

use super::ProtocolError;

pub(super) const MAGIC: [u8; 4] = *b"IMMO";
pub(super) const HEADER_BYTES: usize = 18;
pub(super) const RESPONSE_PAYLOAD_NONE: u8 = 0;
pub(super) const RESPONSE_PAYLOAD_STATUS: u8 = 1;
pub(super) const STATUS_SUPERVISOR_PID: u16 = 1 << 0;
pub(super) const STATUS_MAIN_PID: u16 = 1 << 1;
pub(super) const STATUS_UPTIME: u16 = 1 << 2;
pub(super) const STATUS_DOWN_TIME: u16 = 1 << 3;
pub(super) const STATUS_BACKOFF: u16 = 1 << 4;
pub(super) const STATUS_LAST_RESULT: u16 = 1 << 5;
pub(super) const STATUS_KNOWN_FLAGS: u16 = STATUS_SUPERVISOR_PID
    | STATUS_MAIN_PID
    | STATUS_UPTIME
    | STATUS_DOWN_TIME
    | STATUS_BACKOFF
    | STATUS_LAST_RESULT;

pub(super) struct Cursor<'a> {
    remaining: &'a [u8],
}

impl<'a> Cursor<'a> {
    pub(super) const fn new(bytes: &'a [u8]) -> Self {
        Self { remaining: bytes }
    }

    pub(super) fn take<const N: usize>(&mut self) -> Result<[u8; N], ProtocolError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(N)
            .ok_or(ProtocolError::Truncated)?;
        self.remaining = remaining;
        value.try_into().map_err(|_| ProtocolError::Truncated)
    }

    pub(super) fn byte(&mut self) -> Result<u8, ProtocolError> {
        Ok(self.take::<1>()?.into_iter().next().unwrap_or_default())
    }

    pub(super) fn bytes(&mut self, count: usize) -> Result<&'a [u8], ProtocolError> {
        let (value, remaining) = self
            .remaining
            .split_at_checked(count)
            .ok_or(ProtocolError::Truncated)?;
        self.remaining = remaining;
        Ok(value)
    }

    pub(super) const fn is_empty(&self) -> bool {
        self.remaining.is_empty()
    }

    pub(super) const fn len(&self) -> usize {
        self.remaining.len()
    }
}
