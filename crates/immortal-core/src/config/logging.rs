//! Facade over logging-alias normalization and its compact quantity codec.
//!
//! The `log`, `logger`, `stderr`, and deprecated `logging` fields accepted by
//! [`super::document::ConfigDocument`] each describe local file routing or
//! external logger delegation differently. [`wire`] owns every accepted wire
//! shape and reconciles them into one canonical [`super::model::LoggingConfig`]
//! via [`wire::normalize_logging`]: deprecated v2 shapes are translated only
//! when their stream behavior is representable without widening or dropping
//! output, and every remaining ambiguity becomes a validation error instead
//! of a silent guess. [`quantity`] owns the compact duration and size codec
//! (`1h`, `2MiB`, ...) used to decode `log.age`/`log.size` and to re-encode
//! [`super::model::FileLogConfig`] back to that same canonical spelling.
//! Both children stay private; this facade re-exports exactly the items
//! [`super::document`] and [`super::model`] need.

mod quantity;
mod wire;

pub(super) use self::quantity::{format_log_age, format_log_size};
pub(super) use self::wire::{FileLogInput, LegacyLoggingConfig, LogInput, normalize_logging};
