//! Shared domain library for the immortal executables.
//!
//! This crate deliberately contains only architectural boundaries in the initial
//! skeleton. Public behavior will be added alongside tests as the rewrite grows.

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
pub mod shutdown;
pub mod status;
pub mod supervisor;
pub mod watch;
