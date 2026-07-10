//! Child process creation, daemonization, ownership, environment, and signals.
//!
//! Unix fork, session, daemon, and wait primitives come from the `fork` crate.
//! This module owns the supervisor-facing wrappers so syscall details do not
//! leak into the CLI crates or the rest of the domain model.
