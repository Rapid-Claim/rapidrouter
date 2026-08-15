//! A lock-free token bucket. The consume path is a pure CAS loop over the
//! available count, so concurrent racers can never jointly overdraw —
//! the invariant the loom model checks.
//!
//! Tokens are stored ×1000 ("millitokens") so fractional per-millisecond
//! refill rates stay exact in integers.

use crate::sync::{AtomicU64, Ordering};

#[derive(Debug)]
pub struct TokenBucket {
    /// Available millitokens.
    available: AtomicU64,
    /// Last refill timestamp (caller-supplied ms epoch).
    last_refill_ms: AtomicU64,
    capacity_milli: u64,
    refill_milli_per_ms: u64,
}

impl TokenBucket {
    /// `capacity` tokens, refilled at `per_second` tokens each second,
    /// starting full.
    pub fn new(capacity: u64, per_second: u64) -> Self {
        Self {
            available: AtomicU64::new(capacity * 1000),
            last_refill_ms: AtomicU64::new(0),
            capacity_milli: capacity * 1000,
            refill_milli_per_ms: per_second, // per_second tokens/s == per_second milli/ms
        }
    }

    /// Try to take `tokens` whole tokens. Never overdraws: the CAS retries
    /// until it either owns a sufficient balance or observes one too small.
    pub fn try_consume(&self, tokens: u64, now_ms: u64) -> bool {
        self.refill(now_ms);
        let need = tokens * 1000;
        let mut current = self.available.load(Ordering::Acquire);
        loop {
            if current < need {
                return false;
            }
            match self.available.compare_exchange_weak(
                current,
                current - need,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    /// Credit elapsed time exactly once per interval: whichever racer wins
    /// the CAS on `last_refill_ms` owns that interval's credit.
    fn refill(&self, now_ms: u64) {
        let last = self.last_refill_ms.load(Ordering::Acquire);
        if now_ms <= last {
            return;
        }
        if self
            .last_refill_ms
            .compare_exchange(last, now_ms, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return;
        }
        let credit = (now_ms - last).saturating_mul(self.refill_milli_per_ms);
        let mut current = self.available.load(Ordering::Acquire);
        loop {
            let next = (current + credit).min(self.capacity_milli);
            if next == current {
                return;
            }
            match self.available.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Post-paid debit: drain up to `tokens`, never below zero. Used when
    /// the true cost is only known after the response (token-per-minute
    /// accounting) — the drain blocks subsequent admissions until refill.
    pub fn debit_saturating(&self, tokens: u64, now_ms: u64) {
        self.refill(now_ms);
        let need = tokens.saturating_mul(1000);
        let mut current = self.available.load(Ordering::Acquire);
        loop {
            let next = current.saturating_sub(need);
            if next == current {
                return;
            }
            match self.available.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    /// Duplicate the bucket including its current balance and refill
    /// cursor — used when a table rebuild carries a key's limiter state
    /// into the new snapshot.
    pub fn clone_state(&self) -> Self {
        Self {
            available: AtomicU64::new(self.available.load(Ordering::Acquire)),
            last_refill_ms: AtomicU64::new(self.last_refill_ms.load(Ordering::Acquire)),
            capacity_milli: self.capacity_milli,
            refill_milli_per_ms: self.refill_milli_per_ms,
        }
    }

    pub fn available_tokens(&self) -> u64 {
        self.available.load(Ordering::Acquire) / 1000
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    #[test]
    fn consumes_down_to_zero_and_refuses() {
        let b = TokenBucket::new(3, 0);
        assert!(b.try_consume(1, 0));
        assert!(b.try_consume(2, 0));
        assert!(!b.try_consume(1, 0));
    }

    #[test]
    fn refills_at_rate_and_caps_at_capacity() {
        let b = TokenBucket::new(10, 2); // 2 tokens/sec
        assert!(b.try_consume(10, 0));
        assert!(!b.try_consume(1, 400)); // 0.8 tokens accrued
        assert!(b.try_consume(1, 600)); // 1.2 accrued
        assert!(b.try_consume(5, 10_000)); // long idle: balance capped at capacity
        assert_eq!(b.available_tokens(), 5);
    }

    #[test]
    fn fractional_rates_are_exact() {
        // 1 token per second, checked at sub-second boundaries.
        let b = TokenBucket::new(1, 1);
        assert!(b.try_consume(1, 0));
        assert!(!b.try_consume(1, 999));
        assert!(b.try_consume(1, 1000));
    }
}
