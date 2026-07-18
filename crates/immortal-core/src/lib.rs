//! Shared domain library for the immortal executables.
//!
//! `immortal-core` owns every reusable supervision and operating-system
//! behavior the binaries build on: configuration parsing and validation, the
//! control protocol, process creation and daemonization, the supervision state
//! machine, log routing and rotation, directory reconciliation, and the
//! platform boundary. The `immortal`, `immortalctl`, `immortaldir`, and
//! `immortallog` executables stay thin frontends that translate CLI actions
//! into calls on these modules, so process, protocol, and platform invariants
//! are defined and tested in one place.

pub mod build_info;
pub mod config;
pub mod control;
pub mod executor;
pub mod exit;
pub mod logging;
mod pid_file;
pub mod platform;
pub mod process;
pub mod readiness;
pub mod reconcile;
#[cfg(unix)]
pub mod runtime;
mod service_name;
pub mod shutdown;
pub mod status;
pub mod supervisor;
pub mod watch;
