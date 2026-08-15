//! Loom model checks: the breaker and token bucket hold their invariants
//! under every interleaving loom explores. Run with:
//! `RUSTFLAGS="--cfg loom" cargo test -p router-core --test loom_models --release`
#![cfg(loom)]

use loom::sync::Arc;
use loom::thread;
use router_core::breaker::{Admission, Breaker, BreakerConfig};
use router_core::token_bucket::TokenBucket;

/// Racing consumers can never jointly overdraw the bucket.
#[test]
fn token_bucket_never_overdraws() {
    loom::model(|| {
        let bucket = Arc::new(TokenBucket::new(3, 0));
        let handles: Vec<_> = (0..3)
            .map(|_| {
                let bucket = bucket.clone();
                thread::spawn(move || u64::from(bucket.try_consume(2, 0)) * 2)
            })
            .collect();
        let consumed: u64 = handles.into_iter().map(|h| h.join().unwrap()).sum();
        assert!(
            consumed <= 3,
            "overdraw: {consumed} tokens from a 3-token bucket"
        );
        assert_eq!(consumed + bucket.available_tokens(), 3);
    });
}

/// Refill racing with consumption keeps the balance within
/// [0, capacity + credited] and never mints extra credit.
#[test]
fn token_bucket_refill_race_is_bounded() {
    loom::model(|| {
        let bucket = Arc::new(TokenBucket::new(2, 1000)); // 1 token per ms
        let consumer = {
            let bucket = bucket.clone();
            thread::spawn(move || u64::from(bucket.try_consume(2, 1)) * 2)
        };
        let racer = {
            let bucket = bucket.clone();
            thread::spawn(move || u64::from(bucket.try_consume(1, 1)))
        };
        let spent = consumer.join().unwrap() + racer.join().unwrap();
        // Start 2, credit at most 1ms * 1/ms = 1, cap 2: total inflow <= 3.
        assert!(spent + bucket.available_tokens() <= 3);
    });
}

/// Exactly one racer wins the half-open probe slot.
#[test]
fn breaker_admits_exactly_one_probe() {
    loom::model(|| {
        let breaker = Arc::new(Breaker::new(BreakerConfig {
            failure_threshold: 1,
            window_ms: 100,
            cooldown_ms: 10,
        }));
        breaker.record_failure(0); // open at t=0

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let b = breaker.clone();
                thread::spawn(move || b.admit(20))
            })
            .collect();
        let probes = handles
            .into_iter()
            .map(|h| h.join().unwrap())
            .filter(|a| *a == Admission::Probe)
            .count();
        assert_eq!(probes, 1, "half-open must admit exactly one probe");
    });
}

/// A probe outcome racing new failures never wedges the breaker: after
/// any interleaving it is either open (rejecting) or closed (admitting),
/// and a later success always restores service.
#[test]
fn breaker_recovers_after_any_interleaving() {
    loom::model(|| {
        let breaker = Arc::new(Breaker::new(BreakerConfig {
            failure_threshold: 1,
            window_ms: 100,
            cooldown_ms: 10,
        }));
        breaker.record_failure(0);

        let prober = {
            let b = breaker.clone();
            thread::spawn(move || {
                if b.admit(20) == Admission::Probe {
                    b.record_success(21);
                }
            })
        };
        let failer = {
            let b = breaker.clone();
            thread::spawn(move || b.record_failure(20))
        };
        prober.join().unwrap();
        failer.join().unwrap();

        breaker.record_success(1000);
        assert_eq!(breaker.admit(1001), Admission::Yes);
    });
}
