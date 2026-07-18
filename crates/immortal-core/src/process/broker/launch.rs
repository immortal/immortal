//! Broker fork entry points and pre-runtime construction.
//!
//! [`start_process_broker`], [`start_process_broker_with_lifetime`], and
//! [`start_process_broker_with_logging`] each fork before Tokio or any other
//! thread exists; the child never returns, materializing its logging plan,
//! acquiring the child-subreaper role for its supervised subtree, building a
//! dedicated current-thread runtime, and running the event loop until it exits
//! with a status the parent never observes directly. Acquiring the subreaper
//! role while still single-threaded and pre-runtime keeps that process-wide
//! attribute owned by the one task that reaps the subtree; a platform without
//! the role degrades to ordinary supervision.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream as StdUnixStream;
use std::process;

use tokio::net::UnixStream;
use tokio::runtime::Builder;

use super::ProcessId;
use super::acquire_subreaper;
use super::client::ProcessBrokerEndpoint;
use super::error::ProcessBrokerError;
use super::logging::{BrokerLogging, BrokerLoggingPlan};
use super::runtime::run_broker;
use super::types::BrokerLifetimePlan;

const BROKER_EXIT_SOFTWARE: i32 = 70;

/// Fork a dedicated process broker before Tokio or any other thread is created.
///
/// The child never returns from this function. The parent receives a CLOEXEC
/// endpoint which it registers only after constructing its Tokio runtime.
///
/// # Errors
///
/// Returns a socket-pair or fork error in the supervisor process.
pub fn start_process_broker() -> io::Result<ProcessBrokerEndpoint> {
    start_process_broker_with_logging(BrokerLoggingPlan::default(), None)
}

/// Fork a process broker with one pre-runtime descriptor-cleanup contract.
///
/// The broker invokes this stop command only if its supervisor connection is
/// lost while a descriptor-tracked generation remains active.
///
/// # Errors
///
/// Returns a socket-pair or fork error in the supervisor process.
pub fn start_process_broker_with_lifetime(
    lifetime: BrokerLifetimePlan,
) -> io::Result<ProcessBrokerEndpoint> {
    start_process_broker_with_logging(BrokerLoggingPlan::default(), Some(lifetime))
}

pub(crate) fn start_process_broker_with_logging(
    logging: BrokerLoggingPlan,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
) -> io::Result<ProcessBrokerEndpoint> {
    let pair = fork::socket_pair_cloexec()?;
    let (supervisor_socket, broker_socket) = pair.into_parts();
    match fork::fork_process()? {
        fork::ProcessFork::Parent(process) => {
            drop(broker_socket);
            Ok(ProcessBrokerEndpoint {
                process: ProcessId(process.get()),
                socket: supervisor_socket,
            })
        }
        fork::ProcessFork::Child => {
            drop(supervisor_socket);
            let subreaper_active = acquire_broker_subreaper();
            let exit = match run_broker_process(
                broker_socket,
                logging,
                lifetime_cleanup,
                subreaper_active,
            ) {
                Ok(()) => 0,
                Err(error) => {
                    eprintln!("immortal process broker: {error}");
                    BROKER_EXIT_SOFTWARE
                }
            };
            process::exit(exit);
        }
    }
}

fn run_broker_process(
    socket: OwnedFd,
    logging: BrokerLoggingPlan,
    lifetime_cleanup: Option<BrokerLifetimePlan>,
    subreaper_active: bool,
) -> Result<(), ProcessBrokerError> {
    let logging = BrokerLogging::prepare(logging)?;
    let stream = StdUnixStream::from(socket);
    stream.set_nonblocking(true)?;
    let runtime = Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()?;
    runtime.block_on(async move {
        let stream = UnixStream::from_std(stream)?;
        run_broker(stream, logging, lifetime_cleanup, subreaper_active).await
    })
}

/// Acquire the child-subreaper role for the broker's supervised subtree.
///
/// Returns whether the broker now holds the role. Missing platform support
/// (macOS, older kernels) and a genuine acquisition failure both degrade to
/// ordinary supervision rather than aborting the broker: subreaping only widens
/// orphan containment, so its absence is never fatal to supervision. A real
/// failure is reported on stderr like every other broker diagnostic.
fn acquire_broker_subreaper() -> bool {
    match acquire_subreaper() {
        Ok(status) => status.is_acquired(),
        Err(error) => {
            eprintln!("immortal process broker: subreaper unavailable: {error}");
            false
        }
    }
}
