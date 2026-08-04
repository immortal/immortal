//! Broker event loop: request dispatch, `SIGCHLD` reap sweep, and readiness
//! or lifetime observation delivery.
//!
//! [`run_broker`] is a single `tokio::select!` over the supervisor request
//! stream, the `SIGCHLD` signal (immediate reap), a low-frequency reap
//! interval (closes platform notification gaps), and the readiness/lifetime
//! observation channels populated by detached wait tasks. Supervisor EOF
//! routes to supervisor-loss cleanup instead of returning an error, since
//! losing the connection is an expected, handled transition rather than a
//! broker fault.
//!
//! Every arm of that `select!` must be cancellation-safe, because losing one
//! arm's progress every time a sibling arm wins is the normal case rather than
//! an exception. Frame decoding is not cancellation-safe — it reads a header
//! and then its payload with two separate `read_exact` calls — so the read half
//! is owned exclusively by the [`RequestStream`] reader task and reaches the
//! loop through a bounded channel. That task must not outlive `run_broker`: it
//! holds the only read half, so a lingering task would keep the socket open and
//! defeat supervisor-loss detection. [`RequestStream`]'s `Drop` aborts it on
//! every return path so no new exit can forget to.

use std::collections::BTreeMap;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::signal::unix::{Signal as ChildSignal, SignalKind, signal as listen_for_signal};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{Instant, Interval, MissedTickBehavior, interval_at};

use crate::readiness::ReadinessError;
use crate::supervisor::Generation;

use super::dispatch::handle_broker_request;
use super::error::{ProcessBrokerError, ProcessBrokerErrorKind};
use super::logging::BrokerLogging;
use super::reap::{disarm_generation_guard, forward_child_events};
use super::state::{
    BrokerGeneration, BrokerOwnedProcess, BrokerRuntimeState, LifetimeObservation, LifetimeState,
    ReadinessObservation,
};
use super::supervisor_loss::cleanup_after_supervisor_loss;
use super::types::BrokerLifetimePlan;
use super::wire::{read_request, write_event};
use super::{BrokerEvent, BrokerReadinessFailure, BrokerRequest, ProcessId};

const CHILD_REAP_INTERVAL: Duration = Duration::from_millis(250);
const READINESS_EVENT_CAPACITY: usize = 32;
const LIFETIME_EVENT_CAPACITY: usize = 32;
const REQUEST_CAPACITY: usize = 32;

/// Decoded supervisor requests, plus exclusive ownership of the task decoding them.
///
/// Frame decoding is not cancellation-safe, so it runs in a dedicated task
/// rather than inside the broker `select!`. `Drop` aborts that task, which is
/// what keeps the read half from outliving the loop on any return path.
struct RequestStream {
    requests: mpsc::Receiver<Result<BrokerRequest, ProcessBrokerError>>,
    task: JoinHandle<()>,
}

impl RequestStream {
    /// Take exclusive ownership of `reader` and start decoding requests into a bounded channel.
    fn spawn<R>(mut reader: R) -> Self
    where
        R: AsyncRead + Unpin + Send + 'static,
    {
        let (sender, requests) = mpsc::channel(REQUEST_CAPACITY);
        let task = tokio::spawn(async move {
            loop {
                let request = read_request(&mut reader).await;
                let terminal = request.is_err();
                if sender.send(request).await.is_err() || terminal {
                    return;
                }
            }
        });
        Self { requests, task }
    }

    /// Await the next decoded request, or `None` once the reader task has finished.
    ///
    /// Cancellation-safe: a dropped call loses no buffered request, because the
    /// partially decoded frame lives in the reader task rather than here.
    async fn recv(&mut self) -> Option<Result<BrokerRequest, ProcessBrokerError>> {
        self.requests.recv().await
    }
}

impl Drop for RequestStream {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Mutable event sources and the write endpoint the broker loop owns.
///
/// Grouping them keeps the loop and the supervisor-loss path from passing the
/// same long parameter list, and makes it explicit that cleanup reuses the very
/// endpoints the loop was selecting over.
struct BrokerSession {
    child_reap: Interval,
    child_signal: ChildSignal,
    lifetime_events: mpsc::Receiver<LifetimeObservation>,
    readiness_events: mpsc::Receiver<ReadinessObservation>,
    requests: RequestStream,
    writer: OwnedWriteHalf,
}

pub(super) async fn run_broker(
    stream: UnixStream,
    logging: BrokerLogging,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
    subreaper_active: bool,
) -> Result<(), ProcessBrokerError> {
    let (reader, writer) = stream.into_split();
    let (readiness_sender, readiness_events) = mpsc::channel(READINESS_EVENT_CAPACITY);
    let (lifetime_sender, lifetime_events) = mpsc::channel(LIFETIME_EVENT_CAPACITY);
    let mut child_reap = interval_at(Instant::now() + CHILD_REAP_INTERVAL, CHILD_REAP_INTERVAL);
    child_reap.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut session = BrokerSession {
        child_reap,
        child_signal: listen_for_signal(SignalKind::child())?,
        lifetime_events,
        readiness_events,
        requests: RequestStream::spawn(reader),
        writer,
    };
    let mut state = BrokerRuntimeState {
        generations: BTreeMap::new(),
        logging,
        lifetime_cleanup,
        lifetime_sender,
        processes: BTreeMap::new(),
        readiness_sender,
        subreaper_active,
    };

    // Supervisor loss is handled here rather than at each I/O site so no read,
    // write, or channel-close path can skip the cleanup that runs a
    // descriptor-tracked generation's configured stop command.
    match supervise_generations(&mut session, &mut state).await {
        Ok(()) => Ok(()),
        Err(error) if error.is_end_of_stream() => {
            cleanup_after_supervisor_loss(
                &mut session.child_signal,
                &mut state.generations,
                &mut state.processes,
                &mut state.logging,
                &mut session.lifetime_events,
                state.lifetime_cleanup.take(),
                state.subreaper_active,
            )
            .await
        }
        Err(error) => Err(error),
    }
}

/// Serve the supervisor until it detaches, disconnects, or the broker fails.
///
/// Returns `Ok(())` only for an ordered shutdown the supervisor asked for. A
/// lost connection surfaces as an end-of-stream error so the single caller can
/// run cleanup.
async fn supervise_generations(
    session: &mut BrokerSession,
    state: &mut BrokerRuntimeState,
) -> Result<(), ProcessBrokerError> {
    write_event(&mut session.writer, &BrokerEvent::Ready).await?;

    loop {
        tokio::select! {
            request = session.requests.recv() => {
                let request = match request {
                    Some(Ok(request)) => request,
                    Some(Err(error)) => return Err(error),
                    // The reader task ended without queueing an error, which
                    // still means the supervisor connection is gone.
                    None => return Err(ProcessBrokerError::disconnected()),
                };
                if handle_broker_request(
                    request,
                    &mut session.child_signal,
                    &mut session.writer,
                    state,
                ).await? {
                    return Ok(());
                }
            }
            signal = session.child_signal.recv() => {
                if signal.is_none() {
                    return Err(ProcessBrokerError(ProcessBrokerErrorKind::SignalStreamClosed));
                }
                forward_child_events(
                    &mut session.writer,
                    &mut state.generations,
                    &mut state.processes,
                    &mut state.logging,
                    state.subreaper_active,
                ).await?;
            }
            _ = session.child_reap.tick(), if !state.processes.is_empty() || state.subreaper_active => {
                forward_child_events(
                    &mut session.writer,
                    &mut state.generations,
                    &mut state.processes,
                    &mut state.logging,
                    state.subreaper_active,
                ).await?;
            }
            Some(observation) = session.readiness_events.recv() => {
                if state.generations.contains_key(&observation.generation) {
                    let event = match observation.result {
                        Ok(()) => BrokerEvent::GenerationReady {
                            generation: observation.generation,
                        },
                        Err(error) => BrokerEvent::ReadinessFailed {
                            generation: observation.generation,
                            failure: readiness_failure(&error),
                        },
                    };
                    write_event(&mut session.writer, &event).await?;
                }
            }
            Some(observation) = session.lifetime_events.recv() => {
                handle_lifetime_observation(
                    observation,
                    &mut session.writer,
                    &mut state.generations,
                    &mut state.processes,
                ).await?;
            }
        }
    }
}

async fn handle_lifetime_observation<W>(
    observation: LifetimeObservation,
    writer: &mut W,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<(), ProcessBrokerError>
where
    W: AsyncWrite + Unpin,
{
    if let Some(event) = record_lifetime_observation(&observation, generations, processes)? {
        write_event(writer, &event).await?;
    }
    Ok(())
}

pub(super) fn record_lifetime_observation(
    observation: &LifetimeObservation,
    generations: &mut BTreeMap<Generation, BrokerGeneration>,
    processes: &mut BTreeMap<ProcessId, BrokerOwnedProcess>,
) -> Result<Option<BrokerEvent>, ProcessBrokerError> {
    let Some(entry) = generations.get_mut(&observation.generation) else {
        return Ok(None);
    };
    if entry.lifetime != LifetimeState::Tracking {
        return Ok(None);
    }
    let event = if observation.result.is_ok() {
        entry.lifetime = LifetimeState::Closed;
        BrokerEvent::LifetimeClosed {
            generation: observation.generation,
        }
    } else {
        entry.lifetime = LifetimeState::Failed;
        BrokerEvent::LifetimeFailed {
            generation: observation.generation,
        }
    };
    let generation_finished = entry.child.is_none();
    if generation_finished {
        disarm_generation_guard(observation.generation, generations, processes)?;
        generations.remove(&observation.generation);
    }
    Ok(Some(event))
}

fn readiness_failure(error: &ReadinessError) -> BrokerReadinessFailure {
    match error {
        ReadinessError::Timeout => BrokerReadinessFailure::Timeout,
        ReadinessError::Io(_) => BrokerReadinessFailure::Descriptor,
        ReadinessError::InvalidToken => BrokerReadinessFailure::InvalidToken,
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error;
    use std::ffi::OsString;
    use std::time::Duration;

    use std::io;

    use tokio::io::{AsyncWriteExt, duplex};
    use tokio::net::UnixStream;
    use tokio::sync::oneshot;

    use crate::process::{ProcessCommand, ProcessSignal};
    use crate::supervisor::Generation;

    use super::super::logging::{BrokerLogging, BrokerLoggingPlan};
    use super::super::{BrokerSignalTarget, HEADER_BYTES};
    use super::{BrokerRequest, ProcessBrokerError, RequestStream, run_broker};

    const TEST_DEADLINE: Duration = Duration::from_secs(10);
    /// Cancelled request-arm polls to observe before releasing the payload.
    ///
    /// Each poll yields to the runtime, so this both proves the arm survives
    /// repeated cancellation and gives the reader task room to buffer the
    /// header it has already consumed.
    const CANCELLED_POLLS_BEFORE_PAYLOAD: u32 = 64;

    fn signal_request(value: u64) -> Result<BrokerRequest, Box<dyn Error>> {
        Ok(BrokerRequest::Signal {
            generation: Generation::new(value).ok_or("invalid test generation")?,
            target: BrokerSignalTarget::Group,
            signal: ProcessSignal::Terminate,
        })
    }

    fn spawn_request(value: u64) -> Result<BrokerRequest, Box<dyn Error>> {
        let mut command = ProcessCommand::new(OsString::from("/usr/bin/env"));
        command.argument(OsString::from("--split-frame-regression"));
        Ok(BrokerRequest::Spawn {
            generation: Generation::new(value).ok_or("invalid test generation")?,
            command,
            startup_timeout: Duration::from_millis(1_500),
            readiness_timeout: None,
            lifetime_tracking: false,
        })
    }

    /// Regression: a frame whose header and payload straddle a cancellation.
    ///
    /// The broker loop races the request arm against its reap tick, so the arm
    /// is cancelled far more often than it completes. Decoding inline dropped
    /// the already-consumed header on every cancellation and then misread the
    /// payload as the next frame's header, failing the magic check.
    #[tokio::test]
    async fn request_stream_decodes_a_frame_split_across_cancelled_polls()
    -> Result<(), Box<dyn Error>> {
        let (mut supervisor, broker) = duplex(4096);
        let mut requests = RequestStream::spawn(broker);
        let request = spawn_request(3)?;
        let frame = request.encode()?;
        let (header, payload) = frame
            .split_at_checked(HEADER_BYTES)
            .ok_or("encoded frame is shorter than its header")?;
        assert!(payload.len() > HEADER_BYTES);
        supervisor.write_all(header).await?;
        supervisor.flush().await?;

        let payload = payload.to_vec();
        let (release, released) = oneshot::channel();
        let writer = tokio::spawn(async move {
            released.await?;
            supervisor.write_all(&payload).await?;
            supervisor.flush().await?;
            Ok::<(), Box<dyn Error + Send + Sync>>(())
        });

        let mut release = Some(release);
        let mut cancelled_polls = 0_u32;
        let received = tokio::time::timeout(TEST_DEADLINE, async {
            loop {
                tokio::select! {
                    biased;
                    request = requests.recv() => return request,
                    () = tokio::task::yield_now() => {
                        cancelled_polls = cancelled_polls.saturating_add(1);
                        if cancelled_polls == CANCELLED_POLLS_BEFORE_PAYLOAD
                            && let Some(release) = release.take() {
                            let _ = release.send(());
                        }
                    }
                }
            }
        })
        .await
        .map_err(|_| "timed out awaiting the split frame")?
        .ok_or("request stream ended before delivering the frame")?;
        writer.await?.map_err(|error| error.to_string())?;
        assert_eq!(received?, request);
        assert!(cancelled_polls >= CANCELLED_POLLS_BEFORE_PAYLOAD);
        Ok(())
    }

    #[tokio::test]
    async fn request_stream_preserves_order_across_streamed_frames() -> Result<(), Box<dyn Error>> {
        let (mut supervisor, broker) = duplex(64);
        let mut requests = RequestStream::spawn(broker);
        let sent: Vec<BrokerRequest> =
            (1..=8).map(signal_request).collect::<Result<Vec<_>, _>>()?;
        let expected = sent.clone();
        let writer = tokio::spawn(async move {
            for request in &sent {
                for byte in request.encode()? {
                    supervisor.write_all(&[byte]).await?;
                    supervisor.flush().await?;
                    tokio::task::yield_now().await;
                }
            }
            Ok::<(), Box<dyn Error + Send + Sync>>(())
        });

        let mut received = Vec::new();
        tokio::time::timeout(TEST_DEADLINE, async {
            while received.len() < expected.len() {
                match requests.recv().await {
                    Some(Ok(request)) => received.push(request),
                    Some(Err(error)) => return Err(error.to_string()),
                    None => return Err("request stream ended early".to_owned()),
                }
            }
            Ok(())
        })
        .await
        .map_err(|_| "timed out draining streamed frames")??;
        writer.await?.map_err(|error| error.to_string())?;
        assert_eq!(received, expected);
        Ok(())
    }

    #[tokio::test]
    async fn request_stream_reports_end_of_stream_for_a_truncated_frame()
    -> Result<(), Box<dyn Error>> {
        let (mut supervisor, broker) = duplex(64);
        let mut requests = RequestStream::spawn(broker);
        let frame = signal_request(1)?.encode()?;
        let head = frame
            .split_last()
            .ok_or("encoded frame is empty")?
            .1
            .to_vec();
        supervisor.write_all(&head).await?;
        supervisor.flush().await?;
        drop(supervisor);

        let error = tokio::time::timeout(TEST_DEADLINE, requests.recv())
            .await
            .map_err(|_| "timed out awaiting the truncated frame")?
            .ok_or("request stream ended without reporting truncation")?
            .err()
            .ok_or("truncated frame decoded successfully")?;
        assert!(error.is_end_of_stream());
        Ok(())
    }

    #[tokio::test]
    async fn request_stream_reports_a_protocol_error_for_an_invalid_frame()
    -> Result<(), Box<dyn Error>> {
        let (mut supervisor, broker) = duplex(64);
        let mut requests = RequestStream::spawn(broker);
        let mut frame = signal_request(1)?.encode()?;
        let magic = frame.first_mut().ok_or("encoded frame is empty")?;
        *magic = magic.wrapping_add(1);
        supervisor.write_all(&frame).await?;
        supervisor.flush().await?;

        let error = tokio::time::timeout(TEST_DEADLINE, requests.recv())
            .await
            .map_err(|_| "timed out awaiting the invalid frame")?
            .ok_or("request stream ended without reporting the invalid frame")?
            .err()
            .ok_or("invalid frame decoded successfully")?;
        assert!(!error.is_end_of_stream());
        Ok(())
    }

    #[tokio::test]
    async fn request_stream_ends_after_reporting_a_terminal_error() -> Result<(), Box<dyn Error>> {
        let (supervisor, broker) = duplex(64);
        let mut requests = RequestStream::spawn(broker);
        drop(supervisor);

        let first = tokio::time::timeout(TEST_DEADLINE, requests.recv())
            .await
            .map_err(|_| "timed out awaiting the closed stream")?
            .ok_or("closed stream ended without reporting end of stream")?;
        assert!(first.err().is_some_and(|error| error.is_end_of_stream()));
        let next = tokio::time::timeout(TEST_DEADLINE, requests.recv())
            .await
            .map_err(|_| "timed out awaiting stream completion")?;
        assert!(next.is_none());
        Ok(())
    }

    /// A write to a departed peer is a disconnect, not a broker fault.
    ///
    /// The broker learns the supervisor is gone from whichever direction
    /// notices first. Classifying only the read-side kinds let a `BrokenPipe`
    /// write escape as a fatal error.
    #[test]
    fn broken_pipe_is_classified_as_end_of_stream() {
        let error = ProcessBrokerError::from(io::Error::new(
            io::ErrorKind::BrokenPipe,
            "supervisor closed the connection",
        ));
        assert!(error.is_end_of_stream());
    }

    /// A genuinely unusable socket must stay fatal so the broker fails closed.
    #[test]
    fn other_io_kinds_are_not_classified_as_end_of_stream() {
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::InvalidInput,
            io::ErrorKind::Other,
        ] {
            let error = ProcessBrokerError::from(io::Error::new(kind, "unusable socket"));
            assert!(!error.is_end_of_stream(), "{kind:?} must stay fatal");
        }
    }

    /// Regression: a write that observes the disconnect must reach cleanup.
    ///
    /// Dropping the supervisor half before the broker sends its `Ready` event
    /// makes the very first write fail with `BrokenPipe`, deterministically
    /// ahead of any queued read error. That failure previously propagated out
    /// of `run_broker`, which the launcher reports as a broker fault and which
    /// skips the supervisor-loss cleanup entirely.
    #[tokio::test]
    async fn run_broker_treats_a_broken_pipe_write_as_supervisor_loss() -> Result<(), Box<dyn Error>>
    {
        let (supervisor, broker) = UnixStream::pair()?;
        drop(supervisor);
        let logging = BrokerLogging::prepare(BrokerLoggingPlan {
            local_files: Vec::new(),
            logger: None,
        })?;

        tokio::time::timeout(TEST_DEADLINE, run_broker(broker, logging, None, false))
            .await
            .map_err(|_| "timed out awaiting broker shutdown")??;
        Ok(())
    }
}
