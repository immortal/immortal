//! Shared time and deterministic jitter helpers.
//!
//! Restart and logger backoff use the same bounded arithmetic and deterministic
//! per-generation/per-broker seed mixing. These helpers never inspect process
//! state; they only produce durations for already-authorized lifecycle decisions.

use std::time::{Duration, Instant};

use super::Generation;

pub(super) fn elapsed_seconds(start: Instant) -> u64 {
    start.elapsed().as_secs()
}

pub(super) fn jittered_backoff(
    base_seconds: u64,
    jitter_percent: u8,
    generation: Generation,
    broker: crate::process::ProcessId,
) -> Duration {
    jittered_backoff_seed(base_seconds, jitter_percent, generation.get(), broker)
}

pub(super) fn jittered_backoff_seed(
    base_seconds: u64,
    jitter_percent: u8,
    seed: u64,
    broker: crate::process::ProcessId,
) -> Duration {
    let spread = base_seconds
        .saturating_mul(u64::from(jitter_percent))
        .saturating_div(100);
    if spread == 0 {
        return Duration::from_secs(base_seconds);
    }
    let width = spread.saturating_mul(2).saturating_add(1);
    let broker_seed = u64::try_from(broker.get()).map_or(0, |value| value);
    let mut seed = seed ^ broker_seed;
    seed ^= seed >> 30;
    seed = seed.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    seed ^= seed >> 27;
    seed = seed.wrapping_mul(0x94d0_49bb_1331_11eb);
    seed ^= seed >> 31;
    let offset = seed % width;
    Duration::from_secs(base_seconds.saturating_sub(spread).saturating_add(offset))
}
