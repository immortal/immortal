//! Wire tests for the status fields that explain a failed start: the spawn
//! failure detail of the service and the last result of a logger.

use std::error::Error;

use super::{ProtocolError, Response, ResponseCode};
use crate::{
    process::{SpawnFailure, SpawnStage},
    status::{LastResult, LoggerLastResult, SpawnDetail, StatusSnapshot},
    supervisor::StateMachine,
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
    zero_errno.logger_last_result = Some(LoggerLastResult::SpawnFailed(SpawnDetail {
        os_error: Some(0),
        ..marked_detail()
    }));
    assert!(matches!(
        status_response(zero_errno).encode(),
        Err(ProtocolError::MalformedStatus(_))
    ));

    let mut status = StatusSnapshot::from_machine(&StateMachine::default());
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
