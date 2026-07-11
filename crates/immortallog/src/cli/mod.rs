//! CLI definition and orchestration for `immortallog`.

pub mod actions;
pub mod commands;
pub mod dispatch;

mod start;
pub use start::start;
