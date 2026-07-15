//! CLI definition and orchestration for `immortallog`.

pub mod actions;

mod commands;
mod dispatch;
mod start;

pub use self::start::{StartError, finish, start};
