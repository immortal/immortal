//! Shared domain library for the immortal executables.
//!
//! This crate deliberately contains only architectural boundaries in the initial
//! skeleton. Public behavior will be added alongside tests as the rewrite grows.

pub mod config;
pub mod control;
pub mod logging;
pub mod platform;
pub mod process;
pub mod supervisor;
