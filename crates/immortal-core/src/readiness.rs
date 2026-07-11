//! Bounded readiness notification protocol for `IMMORTAL_READY_FD`.

use std::{
    error::Error,
    fmt::{self, Display, Formatter},
    io,
    time::Duration,
};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::time::timeout;

/// Exact token a service writes once it is ready to receive work.
pub const READY_TOKEN: [u8; 6] = *b"READY\n";

/// Failure while waiting for one generation to declare readiness.
#[derive(Debug)]
pub enum ReadinessError {
    /// Deadline elapsed before the complete token arrived.
    Timeout,
    /// Descriptor closed or failed before a complete token arrived.
    Io(io::Error),
    /// The descriptor delivered a complete but invalid token.
    InvalidToken,
}

impl Display for ReadinessError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("readiness deadline exceeded"),
            Self::Io(error) => write!(formatter, "readiness descriptor failed: {error}"),
            Self::InvalidToken => formatter.write_str("readiness descriptor sent an invalid token"),
        }
    }
}

impl Error for ReadinessError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Timeout | Self::InvalidToken => None,
        }
    }
}

/// Wait for exactly one bounded readiness token.
///
/// Fragmented writes are accepted because the token is read with `read_exact`.
/// Bytes after the token are irrelevant: readiness is a one-way transition for
/// one service generation.
///
/// # Errors
///
/// Returns a timeout, descriptor I/O/EOF failure, or invalid-token error.
pub async fn wait_for_ready<R>(reader: &mut R, deadline: Duration) -> Result<(), ReadinessError>
where
    R: AsyncRead + Unpin,
{
    if deadline.is_zero() {
        return Err(ReadinessError::Timeout);
    }
    let mut received = [0_u8; READY_TOKEN.len()];
    timeout(deadline, reader.read_exact(&mut received))
        .await
        .map_err(|_| ReadinessError::Timeout)?
        .map_err(ReadinessError::Io)?;
    if received == READY_TOKEN {
        Ok(())
    } else {
        Err(ReadinessError::InvalidToken)
    }
}

#[cfg(test)]
mod tests {
    use std::{error::Error, io, time::Duration};

    use tokio::io::{AsyncWriteExt, duplex};

    use super::{READY_TOKEN, ReadinessError, wait_for_ready};

    #[tokio::test(flavor = "current_thread")]
    async fn accepts_fragmented_exact_token() -> Result<(), Box<dyn Error>> {
        let (mut reader, mut writer) = duplex(READY_TOKEN.len());
        let sender = async {
            writer.write_all(b"RE").await?;
            writer.write_all(b"ADY\n").await?;
            Ok::<(), io::Error>(())
        };
        let readiness = wait_for_ready(&mut reader, Duration::from_secs(1));
        let (sent, ready_result) = tokio::join!(sender, readiness);
        sent?;
        ready_result?;
        Ok(())
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_invalid_token_and_early_eof() {
        let mut invalid = b"WRONG\n".as_slice();
        assert!(matches!(
            wait_for_ready(&mut invalid, Duration::from_secs(1)).await,
            Err(ReadinessError::InvalidToken)
        ));

        let mut truncated = b"READY".as_slice();
        assert!(matches!(
            wait_for_ready(&mut truncated, Duration::from_secs(1)).await,
            Err(ReadinessError::Io(_))
        ));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn enforces_deadline_and_rejects_zero_duration() {
        let (mut reader, _writer) = duplex(READY_TOKEN.len());
        assert!(matches!(
            wait_for_ready(&mut reader, Duration::from_millis(5)).await,
            Err(ReadinessError::Timeout)
        ));

        let mut ready = READY_TOKEN.as_slice();
        assert!(matches!(
            wait_for_ready(&mut ready, Duration::ZERO).await,
            Err(ReadinessError::Timeout)
        ));
    }
}
