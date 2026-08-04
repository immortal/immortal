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
//!
//! The child also leaves the supervisor's process group before it owns
//! anything. That group is the terminal's foreground group under
//! `--foreground`, so an inherited group delivers Ctrl-C to the broker, which
//! has no terminal-signal handler; its death fails every containment guard
//! closed and kills the workload immediately instead of letting the supervisor
//! run its ordered shutdown. See [`isolate_broker_process_group`].
//!
//! The two ends form nested reapers. The broker owns and reaps the service
//! subtree while it runs, and the supervisor of the broker acquires the same
//! role so that an abnormal broker exit reparents the broker's surviving
//! children to the supervisor rather than init. The supervisor then reaps those
//! orphans during broker teardown, which is essential on FreeBSD, whose init
//! never reaps a process orphaned from an already-exited reaper and would
//! otherwise leak the workload zombie and its process group permanently.
//!
//! The supervisor must acquire the role *before* forking the broker. On FreeBSD
//! a process inherits its reaper at fork and keeps it even if an ancestor later
//! acquires the role (see [`super::acquire_subreaper`]); acquiring after the
//! fork would leave the broker's reaper set to init, so the broker's orphaned
//! workloads would reparent to init and leak. Acquiring first makes the
//! supervisor the broker's reaper, so the broker's orphans reparent to the
//! supervisor. The role is not inherited across fork, so the broker still
//! acquires its own role for the subtree it owns while alive.

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
    acquire_supervisor_subreaper();
    match fork::fork_process()? {
        fork::ProcessFork::Parent(process) => {
            drop(broker_socket);
            // Also isolate from this side so no terminal signal can reach the
            // broker in the window before it runs its own call.
            let _ = fork::create_process_group(process);
            Ok(ProcessBrokerEndpoint {
                process: ProcessId(process.get()),
                socket: supervisor_socket,
            })
        }
        fork::ProcessFork::Child => {
            drop(supervisor_socket);
            isolate_broker_process_group();
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

/// Move the broker into its own process group before it owns any workload.
///
/// A forked child inherits the supervisor's process group, which under
/// `--foreground` is the terminal's foreground group. Ctrl-C therefore reached
/// the broker as well as the supervisor. The broker installs no terminal-signal
/// handler, so it died by default action, and every containment guard then
/// failed closed and killed its workload group immediately — bypassing the
/// ordered stop command, `SIGTERM`, grace period, and `SIGKILL` escalation the
/// supervisor was about to run. Isolating the group makes the supervisor the
/// only recipient, so it can shut the broker down through the control protocol.
///
/// A failure degrades to the inherited group rather than aborting: supervision
/// still works and only the terminal-signal ordering is lost, whereas losing
/// the broker loses supervision entirely. It is reported on stderr like every
/// other broker diagnostic.
fn isolate_broker_process_group() {
    if let Err(error) = fork::create_current_process_group() {
        eprintln!("immortal process broker: process group isolation unavailable: {error}");
    }
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

/// Acquire the child-subreaper role for the process supervising the broker.
///
/// Call this before forking the broker. The broker owns and reaps the service
/// subtree while it runs, but an abnormal broker exit reparents the broker's
/// surviving children to *the broker's* reaper. On FreeBSD that reaper is fixed
/// when the broker is forked, so acquiring the role first makes the supervisor
/// the broker's reaper: the workload zombies the out-of-group group guard leaves
/// behind then reparent to the supervisor and are reaped during broker teardown.
/// Acquiring after the fork would leave the broker's reaper set to init, whose
/// FreeBSD implementation never reaps a process orphaned from an already-exited
/// reaper, leaking the zombie and its process group forever. Missing platform
/// support degrades to ordinary supervision exactly like the broker's own
/// acquisition; a genuine failure is reported on stderr and, like the broker's,
/// never aborts supervision.
fn acquire_supervisor_subreaper() {
    if let Err(error) = acquire_subreaper() {
        eprintln!("immortal supervisor: subreaper unavailable: {error}");
    }
}
