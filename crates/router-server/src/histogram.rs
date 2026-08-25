//! A latency distribution that survives being summed.
//!
//! Rollups exist so a wide window costs what the *answer* costs rather
//! than what the traffic cost. That works for every figure the console
//! shows except one: a percentile. Sums cannot produce a p95, and
//! averages do not compose across rows — which is why a rollup carrying
//! only `latency_ms_sum` could tell you the mean of a month and never its
//! tail, and why the summary endpoints had to walk raw records instead.
//!
//! Counts in log-spaced buckets do compose. Merging two histograms is
//! adding their columns, which is the only operation a rollup ever
//! performs, and reading a percentile back is a walk over the cumulative
//! counts.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Buckets step by 2^(1/4) — about 19% apart — so a percentile read back
/// from one is within 19% of the truth in the worst case. That is far
/// finer than the console renders and far coarser than storing every
/// value.
const STEPS_PER_OCTAVE: f64 = 4.0;

/// The top bucket, covering roughly four hours and everything past it. A
/// request that took longer than that has a problem no percentile is
/// going to describe.
const MAX_BUCKET: u16 = 96;

/// Latency counts in log-spaced buckets, stored sparsely.
///
/// Sparse because a row's requests land in a handful of buckets, and
/// writing ninety-odd zeroes per row would cost more than the numbers
/// are worth — rollup files are read a day at a time and their size is
/// the whole point of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LatencyHistogram {
    buckets: BTreeMap<u16, u64>,
}

impl LatencyHistogram {
    pub fn is_empty(&self) -> bool {
        self.buckets.is_empty()
    }

    /// How many observations are in here.
    pub fn count(&self) -> u64 {
        self.buckets.values().sum()
    }

    pub fn record(&mut self, ms: u64) {
        *self.buckets.entry(bucket_of(ms)).or_insert(0) += 1;
    }

    pub fn merge(&mut self, other: &Self) {
        for (bucket, count) in &other.buckets {
            *self.buckets.entry(*bucket).or_insert(0) += count;
        }
    }

    /// Nearest-rank percentile, in milliseconds.
    ///
    /// Zero for an empty set, which reads as "no data" rather than a
    /// confident zero-millisecond p95 — the same choice the raw-record
    /// path makes.
    ///
    /// A bucket is reported at its upper bound, so the answer errs
    /// *slow*. For latency that is the safe direction: a gateway that
    /// rounds its p95 down is one that under-reports the problem an
    /// operator opened the page to find.
    pub fn percentile(&self, p: u64) -> u64 {
        let total = self.count();
        if total == 0 {
            return 0;
        }
        let rank = ((p as f64 / 100.0) * total as f64).ceil().max(1.0) as u64;
        let mut seen = 0;
        for (bucket, count) in &self.buckets {
            seen += count;
            if seen >= rank {
                return upper_bound_ms(*bucket);
            }
        }
        // Only reachable through float rounding at p = 100.
        self.buckets
            .keys()
            .next_back()
            .copied()
            .map(upper_bound_ms)
            .unwrap_or(0)
    }
}

impl FromIterator<u64> for LatencyHistogram {
    fn from_iter<T: IntoIterator<Item = u64>>(values: T) -> Self {
        let mut out = Self::default();
        for ms in values {
            out.record(ms);
        }
        out
    }
}

/// Which bucket a millisecond reading falls in.
///
/// Bucket 0 is "under a millisecond", which is its own answer rather
/// than the bottom of a logarithmic scale that has no bottom.
fn bucket_of(ms: u64) -> u16 {
    if ms == 0 {
        return 0;
    }
    let index = (STEPS_PER_OCTAVE * (ms as f64).log2()).floor() as i64 + 1;
    index.clamp(1, MAX_BUCKET as i64) as u16
}

/// The slowest reading a bucket can hold, in milliseconds.
fn upper_bound_ms(bucket: u16) -> u64 {
    if bucket == 0 {
        return 0;
    }
    2f64.powf(bucket as f64 / STEPS_PER_OCTAVE).ceil() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_percentile_lands_within_a_bucket_of_the_truth() {
        // 1..=1000 ms, so the true p50 is 500 and the true p95 is 950.
        let hist: LatencyHistogram = (1..=1000).collect();
        let p50 = hist.percentile(50);
        let p95 = hist.percentile(95);
        // Reported at the bucket's upper bound, so never under.
        assert!(p50 >= 500, "p50 {p50} should not under-report");
        assert!(p95 >= 950, "p95 {p95} should not under-report");
        assert!(p50 <= 500 * 6 / 5, "p50 {p50} drifted more than a bucket");
        assert!(p95 <= 950 * 6 / 5, "p95 {p95} drifted more than a bucket");
    }

    #[test]
    fn merging_is_the_same_as_recording_both_sides() {
        let mut merged: LatencyHistogram = (1..=500).collect();
        merged.merge(&(501..=1000).collect());
        let together: LatencyHistogram = (1..=1000).collect();
        assert_eq!(merged, together);
    }

    #[test]
    fn an_empty_histogram_reports_no_data_rather_than_zero_latency() {
        let hist = LatencyHistogram::default();
        assert_eq!(hist.count(), 0);
        assert_eq!(hist.percentile(50), 0);
        assert_eq!(hist.percentile(95), 0);
        assert!(hist.is_empty());
    }

    #[test]
    fn one_observation_is_its_own_every_percentile() {
        let hist: LatencyHistogram = [420].into_iter().collect();
        for p in [1, 50, 95, 99, 100] {
            let value = hist.percentile(p);
            assert!((420..=504).contains(&value), "p{p} was {value}");
        }
    }

    #[test]
    fn sub_millisecond_and_absurd_readings_both_land_somewhere() {
        let hist: LatencyHistogram = [0, u64::MAX].into_iter().collect();
        assert_eq!(hist.count(), 2);
        assert_eq!(hist.percentile(1), 0);
        assert_eq!(hist.percentile(100), upper_bound_ms(MAX_BUCKET));
    }

    #[test]
    fn buckets_stay_sparse() {
        let hist: LatencyHistogram = (1..=100_000).collect();
        // Five octaves of readings, four buckets each, plus slack.
        assert!(hist.buckets.len() <= 72, "{} buckets", hist.buckets.len());
    }

    #[test]
    fn it_round_trips_through_json() {
        let hist: LatencyHistogram = (1..=1000).collect();
        let text = serde_json::to_string(&hist).expect("serializes");
        let back: LatencyHistogram = serde_json::from_str(&text).expect("deserializes");
        assert_eq!(hist, back);
    }

    #[test]
    fn an_absent_histogram_reads_as_empty() {
        #[derive(Deserialize)]
        struct Row {
            #[serde(default)]
            latency: LatencyHistogram,
        }
        // A rollup file written before histograms existed.
        let row: Row = serde_json::from_str("{}").expect("deserializes");
        assert!(row.latency.is_empty());
    }
}
