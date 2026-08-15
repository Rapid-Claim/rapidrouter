//! Per-key circuit breaker: closed / open / half-open.
//!
//! Time enters as caller-supplied milliseconds (a monotonic process
//! epoch), which keeps this type clock-free: production passes real
//! elapsed time, tests pass whatever schedule they need, and loom models
//! interleavings without a clock at all.
//!
//! State and its timestamp live in ONE atomic word — `(since_ms << 2) |
//! state` — because a two-word design has a real race: a thread that
//! observes the new state but the old timestamp will admit a second
//! half-open probe. Every transition is a single CAS on the packed word.

use crate::sync::{AtomicU32, AtomicU64, Ordering};

const CLOSED: u64 = 0;
const OPEN: u64 = 1;
const HALF_OPEN: u64 = 2;

#[inline]
fn pack(since_ms: u64, state: u64) -> u64 {
    (since_ms << 2) | state
}

#[inline]
fn unpack(word: u64) -> (u64, u64) {
    (word >> 2, word & 0b11)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Healthy: send the request.
    Yes,
    /// This caller won the half-open probe slot; its outcome decides the
    /// breaker's next state.
    Probe,
    /// Rejected without a request.
    No,
}

#[derive(Debug, Clone, Copy)]
pub struct BreakerConfig {
    pub failure_threshold: u32,
    pub window_ms: u64,
    pub cooldown_ms: u64,
}

#[derive(Debug)]
pub struct Breaker {
    /// `(since_ms << 2) | state`. `since_ms`: when it opened (OPEN) or
    /// when the current probe was admitted (HALF_OPEN).
    word: AtomicU64,
    failures: AtomicU32,
    window_start_ms: AtomicU64,
    config: BreakerConfig,
}

impl Breaker {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            word: AtomicU64::new(pack(0, CLOSED)),
            failures: AtomicU32::new(0),
            window_start_ms: AtomicU64::new(0),
            config,
        }
    }

    /// Non-mutating view: selection uses this to prefer healthy keys
    /// without consuming anyone's probe slot.
    pub fn looks_healthy(&self) -> bool {
        unpack(self.word.load(Ordering::Acquire)).1 == CLOSED
    }

    pub fn admit(&self, now_ms: u64) -> Admission {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let (since, state) = unpack(current);
            match state {
                CLOSED => return Admission::Yes,
                _ => {
                    // OPEN past cooldown, or HALF_OPEN whose probe has
                    // been out a full cooldown (prober presumed dead):
                    // claim the probe slot with a fresh timestamp.
                    if now_ms.saturating_sub(since) < self.config.cooldown_ms {
                        return Admission::No;
                    }
                    match self.word.compare_exchange_weak(
                        current,
                        pack(now_ms, HALF_OPEN),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return Admission::Probe,
                        Err(actual) => current = actual,
                    }
                }
            }
        }
    }

    pub fn record_success(&self, _now_ms: u64) {
        self.failures.store(0, Ordering::Release);
        self.word.store(pack(0, CLOSED), Ordering::Release);
    }

    pub fn record_failure(&self, now_ms: u64) {
        let mut current = self.word.load(Ordering::Acquire);
        loop {
            let (_, state) = unpack(current);
            match state {
                OPEN => return,
                HALF_OPEN => {
                    // Failed probe: reopen with a fresh cooldown.
                    match self.word.compare_exchange_weak(
                        current,
                        pack(now_ms, OPEN),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return,
                        Err(actual) => current = actual,
                    }
                }
                _ => {
                    // CLOSED: windowed counting. The count itself may be
                    // approximate under races; the safety-critical
                    // invariants (single probe, recovery) live in `word`.
                    let window_start = self.window_start_ms.load(Ordering::Acquire);
                    let count = if now_ms.saturating_sub(window_start) > self.config.window_ms
                        && self
                            .window_start_ms
                            .compare_exchange(
                                window_start,
                                now_ms,
                                Ordering::AcqRel,
                                Ordering::Acquire,
                            )
                            .is_ok()
                    {
                        self.failures.store(1, Ordering::Release);
                        1
                    } else {
                        self.failures.fetch_add(1, Ordering::AcqRel) + 1
                    };
                    if count < self.config.failure_threshold {
                        return;
                    }
                    match self.word.compare_exchange_weak(
                        current,
                        pack(now_ms, OPEN),
                        Ordering::AcqRel,
                        Ordering::Acquire,
                    ) {
                        Ok(_) => return,
                        Err(actual) => current = actual,
                    }
                }
            }
        }
    }

    pub fn is_open(&self) -> bool {
        !self.looks_healthy()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn breaker() -> Breaker {
        Breaker::new(BreakerConfig {
            failure_threshold: 3,
            window_ms: 1000,
            cooldown_ms: 500,
        })
    }

    #[test]
    fn opens_after_threshold_within_window() {
        let b = breaker();
        b.record_failure(0);
        b.record_failure(10);
        assert_eq!(b.admit(20), Admission::Yes);
        b.record_failure(20);
        assert_eq!(b.admit(30), Admission::No);
    }

    #[test]
    fn window_expiry_resets_the_count() {
        let b = breaker();
        b.record_failure(0);
        b.record_failure(10);
        b.record_failure(2000); // fresh window: count restarts at 1
        b.record_failure(2010);
        assert_eq!(b.admit(2020), Admission::Yes);
        b.record_failure(2020);
        assert_eq!(b.admit(2030), Admission::No);
    }

    #[test]
    fn cooldown_admits_single_probe_then_success_closes() {
        let b = breaker();
        for t in [0, 1, 2] {
            b.record_failure(t);
        }
        assert_eq!(b.admit(100), Admission::No);
        assert_eq!(b.admit(600), Admission::Probe);
        assert_eq!(b.admit(601), Admission::No); // probe slot taken
        b.record_success(650);
        assert_eq!(b.admit(651), Admission::Yes);
    }

    #[test]
    fn failed_probe_reopens_with_fresh_cooldown() {
        let b = breaker();
        for t in [0, 1, 2] {
            b.record_failure(t);
        }
        assert_eq!(b.admit(600), Admission::Probe);
        b.record_failure(700);
        assert_eq!(b.admit(1100), Admission::No); // cooldown restarted at 700
        assert_eq!(b.admit(1250), Admission::Probe);
    }

    #[test]
    fn stuck_probe_is_replaced_after_another_cooldown() {
        let b = breaker();
        for t in [0, 1, 2] {
            b.record_failure(t);
        }
        assert_eq!(b.admit(600), Admission::Probe);
        // The prober vanished; a cooldown later someone else may probe.
        assert_eq!(b.admit(1200), Admission::Probe);
    }

    #[test]
    fn success_always_recovers_fully() {
        let b = breaker();
        for t in [0, 1, 2] {
            b.record_failure(t);
        }
        b.record_success(600);
        assert_eq!(b.admit(601), Admission::Yes);
        b.record_failure(700);
        b.record_failure(701);
        assert_eq!(b.admit(702), Admission::Yes);
    }
}
