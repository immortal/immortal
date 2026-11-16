//! Wire tests for the status fields that explain a failed start: the spawn
//! failure detail of the service and the last result of a logger.

use std::error::Error;

use super::{HEADER_BYTES, MAX_FRAME_BYTES, ProtocolError, Response, ResponseCode};
use crate::{
    process::{SpawnFailure, SpawnStage},
    status::{LastResult, LoggerLastResult, LoggerStatus, SpawnDetail, StatusSnapshot},
    supervisor::{Generation, StateMachine},
};

fn status_response(status: StatusSnapshot) -> Response {
    Response {
        code: ResponseCode::Ok,
        generation: None,
        message: "status".to_owned(),
        status: Some(status),
    }
}

/// An errno no other field of the test frame contains, so its bytes locate
/// the encoded spawn detail.
const MARKER_ERRNO: i32 = 0x7a7b_7c7d;

fn marked_detail() -> SpawnDetail {
    SpawnDetail {
        stage: SpawnStage::Execute,
        failure: SpawnFailure::OperatingSystem,
        os_error: Some(MARKER_ERRNO),
    }
}

/// The reason a service or logger could not start must cross the wire intact
/// in every shape, including a failure that carried no errno.
#[test]
fn status_spawn_and_logger_failure_details_round_trip() -> Result<(), Box<dyn Error>> {
    let without_errno = SpawnDetail {
        stage: SpawnStage::StartupHandshake,
        failure: SpawnFailure::TimedOut,
        os_error: None,
    };
    for logger in [
        None,
        Some(LoggerLastResult::SpawnFailed(marked_detail())),
        Some(LoggerLastResult::SpawnFailed(without_errno)),
        Some(LoggerLastResult::Exited(3)),
        Some(LoggerLastResult::Signaled(9)),
    ] {
        for spawn in [None, Some(marked_detail()), Some(without_errno)] {
            let mut status = StatusSnapshot::from_machine(&StateMachine::default());
            status.last_result = Some(LastResult::SpawnFailed);
            status.last_spawn_failure = spawn;
            status.logger = LoggerStatus::Backoff;
            status.logger_last_result = logger;
            let response = status_response(status);
            assert_eq!(Response::decode(&response.encode()?)?, response);
        }
    }
    Ok(())
}

/// A status without the new details encodes exactly as before they existed,
/// so a supervisor that has nothing to explain stays readable by any client.
#[test]
fn status_without_failure_details_keeps_the_previous_frame() -> Result<(), Box<dyn Error>> {
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.last_result = Some(LastResult::SpawnFailed);
    status.logger = LoggerStatus::Backoff;
    let plain = status_response(status.clone()).encode()?;
    status.logger_last_result = Some(LoggerLastResult::Exited(1));
    let detailed = status_response(status).encode()?;
    assert_eq!(detailed.len(), plain.len() + 2);
    let decoded = Response::decode(&plain)?;
    let decoded = decoded.status.ok_or("status payload missing")?;
    assert_eq!(decoded.last_spawn_failure, None);
    assert_eq!(decoded.logger_last_result, None);
    Ok(())
}

/// Malformed or inconsistent failure details are rejected, never guessed at.
#[test]
fn status_failure_details_reject_malformed_input() -> Result<(), Box<dyn Error>> {
    let mut inconsistent = StatusSnapshot::from_machine(&StateMachine::default());
    inconsistent.last_result = Some(LastResult::Exited(1));
    inconsistent.last_spawn_failure = Some(marked_detail());
    assert!(matches!(
        status_response(inconsistent).encode(),
        Err(ProtocolError::MalformedStatus(_))
    ));

    let mut zero_errno = StatusSnapshot::from_machine(&StateMachine::default());
    zero_errno.logger = LoggerStatus::Failed;
    zero_errno.logger_last_result = Some(LoggerLastResult::SpawnFailed(SpawnDetail {
        os_error: Some(0),
        ..marked_detail()
    }));
    assert!(matches!(
        status_response(zero_errno).encode(),
        Err(ProtocolError::MalformedStatus(_))
    ));

    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.logger = LoggerStatus::Failed;
    status.logger_last_result = Some(LoggerLastResult::SpawnFailed(marked_detail()));
    let frame = status_response(status).encode()?;
    let marker = MARKER_ERRNO.to_be_bytes();
    let errno_at = frame
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or("encoded errno not found")?;
    // Stage, classification, and the logger result kind precede the errno.
    for (offset, name) in [(2, "stage"), (1, "classification"), (3, "result kind")] {
        let mut corrupted = frame.clone();
        let index = errno_at.checked_sub(offset).ok_or("detail offset")?;
        *corrupted.get_mut(index).ok_or("detail byte")? = 0xff;
        assert!(
            matches!(
                Response::decode(&corrupted),
                Err(ProtocolError::MalformedStatus(_))
            ),
            "an unknown {name} was accepted"
        );
    }
    let truncated = frame.get(..frame.len() - 1).ok_or("frame too short")?;
    assert!(Response::decode(truncated).is_err());
    Ok(())
}

/// A status frame exactly as the codec at commit `3befcfc` encoded it, before
/// the failure details existed: every earlier optional field is present and
/// neither new flag is set.
const FRAME_BEFORE_FAILURE_DETAILS: &str = "494d4d4f01000100000000000000000600550006737461747573003f\
     0101000300001092000010f7000000000000000700000000000000090000000000000003000000000000000200\
     000000000000050300000300072f62696e2f736800022d63000474727565";

fn status_before_failure_details() -> Response {
    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
    status.supervisor_pid = Some(4242);
    status.main_pid = Some(4343);
    status.uptime_seconds = Some(7);
    status.down_seconds = Some(9);
    status.starts = 3;
    status.failures = 2;
    status.last_result = Some(LastResult::SpawnFailed);
    status.backoff_seconds = Some(5);
    status.logger = LoggerStatus::Backoff;
    status.command = vec!["/bin/sh".to_owned(), "-c".to_owned(), "true".to_owned()];
    Response {
        generation: Generation::new(6),
        ..status_response(status)
    }
}

fn bytes_from_hex(hex: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let digits: Vec<u8> = hex.bytes().filter(u8::is_ascii_hexdigit).collect();
    digits
        .chunks(2)
        .map(|pair| Ok(u8::from_str_radix(std::str::from_utf8(pair)?, 16)?))
        .collect()
}

/// Cut a frame to `length` bytes and make its header announce that length,
/// so decoding reaches the cut instead of rejecting the envelope.
fn cut_with_valid_envelope(frame: &[u8], length: usize) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut cut = frame.get(..length).ok_or("cut beyond the frame")?.to_vec();
    let payload = u16::try_from(
        length
            .checked_sub(HEADER_BYTES)
            .ok_or("cut inside the header")?,
    )?;
    cut.get_mut(HEADER_BYTES - 2..HEADER_BYTES)
        .ok_or("frame shorter than its header")?
        .copy_from_slice(&payload.to_be_bytes());
    Ok(cut)
}

/// A supervisor with nothing to explain must send the bytes the released
/// codec sent, and a released frame must still decode: this is what keeps an
/// older client and a newer supervisor compatible in that case.
#[test]
fn status_without_failure_details_matches_the_released_frame() -> Result<(), Box<dyn Error>> {
    let released = bytes_from_hex(FRAME_BEFORE_FAILURE_DETAILS)?;
    let response = status_before_failure_details();
    assert_eq!(response.encode()?, released);
    assert_eq!(Response::decode(&released)?, response);
    Ok(())
}

/// Every proper prefix of a frame carrying both details is rejected as
/// truncated, including a cut inside either detail, even when the header
/// announces exactly the bytes that remain.
#[test]
fn status_failure_details_reject_every_truncation() -> Result<(), Box<dyn Error>> {
    let mut response = status_before_failure_details();
    let status = response.status.as_mut().ok_or("status payload missing")?;
    status.last_spawn_failure = Some(marked_detail());
    status.logger_last_result = Some(LoggerLastResult::SpawnFailed(marked_detail()));
    let frame = response.encode()?;
    assert_eq!(Response::decode(&frame)?, response);

    for length in HEADER_BYTES..frame.len() {
        let cut = cut_with_valid_envelope(&frame, length)?;
        assert!(
            matches!(Response::decode(&cut), Err(ProtocolError::Truncated)),
            "a frame cut to {length} of {} bytes was not rejected as truncated",
            frame.len()
        );
    }
    Ok(())
}

/// The decoder enforces the same consistency the encoder does, so a peer
/// cannot deliver a detail the result beside it contradicts.
#[test]
fn status_decoder_rejects_details_their_context_contradicts() -> Result<(), Box<dyn Error>> {
    let mut response = status_before_failure_details();
    let status = response.status.as_mut().ok_or("status payload missing")?;
    status.last_spawn_failure = Some(marked_detail());
    let frame = response.encode()?;
    let marker = MARKER_ERRNO.to_be_bytes();
    let errno_at = frame
        .windows(marker.len())
        .position(|window| window == marker)
        .ok_or("encoded errno not found")?;
    // The last-result kind sits before its value, the stage, and the class.
    let mut exited = frame.clone();
    let kind = errno_at.checked_sub(4).ok_or("result offset")?;
    *exited.get_mut(kind).ok_or("result byte")? = 1;
    assert!(matches!(
        Response::decode(&exited),
        Err(ProtocolError::MalformedStatus(_))
    ));

    for healthy in [LoggerStatus::Ready, LoggerStatus::NotConfigured] {
        let mut response = status_before_failure_details();
        let status = response.status.as_mut().ok_or("status payload missing")?;
        status.logger_last_result = Some(LoggerLastResult::Exited(1));
        let mut frame = response.encode()?;
        let status = response.status.as_mut().ok_or("status payload missing")?;
        status.logger = healthy;
        assert!(matches!(
            response.encode(),
            Err(ProtocolError::MalformedStatus(_))
        ));
        // Flags, desired state, service state, and readiness precede it.
        let logger_at = HEADER_BYTES + 2 + "status".len() + 5;
        *frame.get_mut(logger_at).ok_or("logger byte")? = healthy.code();
        assert!(
            matches!(
                Response::decode(&frame),
                Err(ProtocolError::MalformedStatus(_))
            ),
            "a logger failure was accepted beside {}",
            healthy.name()
        );
    }
    Ok(())
}

/// Both details count toward the frame bound: a status which fills the frame
/// exactly round-trips, and one more byte is refused instead of sent.
#[test]
fn status_failure_details_respect_the_frame_bound() -> Result<(), Box<dyn Error>> {
    let mut response = status_before_failure_details();
    let status = response.status.as_mut().ok_or("status payload missing")?;
    status.last_spawn_failure = Some(marked_detail());
    status.logger_last_result = Some(LoggerLastResult::SpawnFailed(marked_detail()));
    status.command = vec![String::new()];
    let room = MAX_FRAME_BYTES - response.encode()?.len();

    let status = response.status.as_mut().ok_or("status payload missing")?;
    status.command = vec!["a".repeat(room)];
    let frame = response.encode()?;
    assert_eq!(frame.len(), MAX_FRAME_BYTES);
    assert_eq!(Response::decode(&frame)?, response);

    let status = response.status.as_mut().ok_or("status payload missing")?;
    status.command = vec!["a".repeat(room + 1)];
    assert!(matches!(
        response.encode(),
        Err(ProtocolError::FrameTooLarge(_))
    ));
    Ok(())
}
