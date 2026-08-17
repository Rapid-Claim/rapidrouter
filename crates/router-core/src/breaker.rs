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
    /// Bench deadline: a hard "not before" the provider gave us, in the
    /// same millisecond epoch as `admit`. `0` means not benched.
    ///
    /// Deliberately a second word rather than more bits in `word`. The
    /// invariant `word` protects is *exactly one* half-open probe, and
    /// that needs a single CAS; a bench deadline needs no such agreement.
    /// The worst a race here can do is let one request through as a bench
    /// is being applied, or apply a bench a moment late — both of which
    /// the next response corrects. Widening `word` to carry a deadline
    /// would shrink the timestamp and complicate the probe CAS to protect
    /// something that does not need protecting.
    bench_until_ms: AtomicU64,
    config: BreakerConfig,
}

impl Breaker {
    pub fn new(config: BreakerConfig) -> Self {
        Self {
            word: AtomicU64::new(pack(0, CLOSED)),
            failures: AtomicU32::new(0),
            window_start_ms: AtomicU64::new(0),
            bench_until_ms: AtomicU64::new(0),
            config,
        }
    }

    /// Non-mutating view: selection uses this to prefer healthy keys
    /// without consuming anyone's probe slot.
    ///
    /// A benched key never looks healthy, so selection skips it without
    /// needing to know why it is out — but only while the bench is still
    /// running. `now_ms` is required for exactly that reason: reading the
    /// deadline alone treated an expired bench as current, so a seat whose
    /// window had rolled stayed out of the healthy pool until some other
    /// path happened to call [`Self::admit`] and clear the flag. That made
    /// the first request after every recovery fail against a pool that
    /// had capacity.
    pub fn looks_healthy(&self, now_ms: u64) -> bool {
        unpack(self.word.load(Ordering::Acquire)).1 == CLOSED && !self.is_benched(now_ms)
    }

    /// Whether a bench is set *and* has not yet elapsed.
    pub fn is_benched(&self, now_ms: u64) -> bool {
        match self.bench_until_ms.load(Ordering::Acquire) {
            0 => false,
            deadline => now_ms < deadline,
        }
    }

    /// Bench this key until `until_ms`, no matter what the breaker's own
    /// cooldown would have said.
    ///
    /// This is the subscription path: a rate-limited seat is out until the
    /// provider's rolling window rolls, which can be hours. The deadline
    /// only ever moves later — two concurrent 429s reporting different
    /// windows must not let the shorter one shorten the bench.
    pub fn bench_until(&self, until_ms: u64) {
        self.bench_until_ms.fetch_max(until_ms, Ordering::AcqRel);
    }

    /// When this key comes off the bench, or `None` if it is not benched.
    /// Observability only — `admit` is the authority.
    pub fn benched_until_ms(&self) -> Option<u64> {
        match self.bench_until_ms.load(Ordering::Acquire) {
            0 => None,
            deadline => Some(deadline),
        }
    }

    /// A one-word health summary for the console. Observability only —
    /// [`Self::admit`] remains the authority on what actually serves.
    ///
    /// `benched` outranks `open` because it is the more specific and more
    /// actionable answer: an operator seeing "open" reaches for the
    /// breaker's cooldown, when the truth is a provider quota window they
    /// cannot shorten.
    pub fn health(&self, now_ms: u64) -> &'static str {
        if self.is_benched(now_ms) {
            return "benched";
        }
        match unpack(self.word.load(Ordering::Acquire)).1 {
            CLOSED => "healthy",
            HALF_OPEN => "probing",
            _ => "open",
        }
    }

    pub fn admit(&self, now_ms: u64) -> Admission {
        // A provider-declared bench outranks the breaker's own state: no
        // probe, no half-open slot, nothing until the window rolls.
        // Probing early is not free — it costs the caller a retry
        // attempt and earns another 429.
        let bench = self.bench_until_ms.load(Ordering::Acquire);
        if bench != 0 {
            if now_ms < bench {
                return Admission::No;
            }
            // Expired. Clear it so the key rejoins on the breaker's own
            // terms; losing this CAS means another thread cleared it
            // first, which is the same outcome.
            let _ =
                self.bench_until_ms
                    .compare_exchange(bench, 0, Ordering::AcqRel, Ordering::Acquire);
        }
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
        // A success means the window rolled early (or the bench was for a
        // different reason than we thought); either way the provider just
        // served us, so stop holding the key out.
        self.bench_until_ms.store(0, Ordering::Release);
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

    /// Observability only. Takes `now_ms` so an elapsed bench does not
    /// read as open forever.
    pub fn is_open(&self, now_ms: u64) -> bool {
        !self.looks_healthy(now_ms)
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
    fn bench_outranks_a_closed_breaker() {
        let b = breaker();
        assert_eq!(b.admit(0), Admission::Yes);
        b.bench_until(5_000);
        assert!(!b.looks_healthy(100), "a benched key must not look healthy");
        assert_eq!(b.admit(100), Admission::No);
        // Not even a probe — the breaker's cooldown is irrelevant here.
        assert_eq!(b.admit(4_999), Admission::No);
        assert_eq!(b.admit(5_000), Admission::Yes);
    }

    #[test]
    fn bench_outranks_an_open_breaker_and_its_probe_slot() {
        let b = breaker();
        for t in [0, 1, 2] {
            b.record_failure(t);
        }
        b.bench_until(10_000);
        // The breaker alone would have offered a probe at 500ms.
        assert_eq!(b.admit(600), Admission::No);
        assert_eq!(b.admit(9_999), Admission::No);
        // Once the bench expires the breaker's own logic resumes.
        assert_eq!(b.admit(10_000), Admission::Probe);
    }

    #[test]
    fn bench_deadline_only_moves_later() {
        let b = breaker();
        b.bench_until(9_000);
        b.bench_until(2_000); // a shorter window must not shorten the bench
        assert_eq!(b.admit(3_000), Admission::No);
        assert_eq!(b.benched_until_ms(), Some(9_000));
        b.bench_until(12_000);
        assert_eq!(b.admit(9_500), Admission::No);
        assert_eq!(b.benched_until_ms(), Some(12_000));
    }

    #[test]
    fn success_clears_the_bench() {
        let b = breaker();
        b.bench_until(60_000);
        b.record_success(100);
        assert_eq!(b.benched_until_ms(), None);
        assert_eq!(b.admit(200), Admission::Yes);
    }

    #[test]
    fn an_unbenched_key_reports_no_deadline() {
        let b = breaker();
        assert_eq!(b.benched_until_ms(), None);
        b.bench_until(1_000);
        // Expiry is observed through admit, which clears it.
        assert_eq!(b.admit(1_000), Admission::Yes);
        assert_eq!(b.benched_until_ms(), None);
    }

    /// A bench that has elapsed is not a bench. Reading the deadline
    /// without comparing it to now kept a recovered seat out of the
    /// healthy pool until something else happened to call `admit`, which
    /// made the first request after every recovery fail.
    #[test]
    fn an_elapsed_bench_stops_holding_the_key_out() {
        let b = breaker();
        b.bench_until(5_000);
        assert!(!b.looks_healthy(4_999), "still benched a moment before");
        assert!(b.is_benched(4_999));
        assert_eq!(b.admit(4_999), Admission::No);

        assert!(!b.is_benched(5_000), "the deadline is not still out");
        assert!(b.looks_healthy(5_000), "selection must see it again");
        assert_eq!(b.health(5_000), "healthy");
        assert_eq!(b.admit(5_000), Admission::Yes);
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
