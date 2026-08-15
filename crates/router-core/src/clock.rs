//! Monotonic milliseconds since process start — the time base for
//! breakers and token buckets.

use std::sync::OnceLock;
use std::time::Instant;

pub fn now_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}
