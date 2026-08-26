//! A derived, disposable read tier over the rollup deltas.
//!
//! The flusher writes one rollup file per batch — every ten seconds by
//! default, so up to 8,640 files a day, per node. That is the right shape
//! for a *writer*: each file is opened once, written once and never
//! reopened, so a crash costs at most one batch. It is a terrible shape
//! for a reader. `history` opened every one of them, which was already
//! slow over thirty days and is roughly three million file opens over a
//! year — the thing that made a year of retention unreadable rather than
//! merely large.
//!
//! The obvious fix is to merge the deltas and delete them. That is wrong
//! here, and the reason is worth writing down: shipped rollup objects are
//! de-duplicated against local *file names* (`fleet_rollups`), so a node
//! that renamed its local files would stop recognising its own uploads
//! and would add them to the rows it had just read — double-counting
//! every deployment's spend.
//!
//! So nothing is merged and nothing is deleted. This is a cache beside
//! the deltas, holding the same rows folded, and it is only ever *read*
//! when its fingerprint still matches the directory it was built from. A
//! stale or missing cache costs a fallback to the slow path, never a
//! wrong number, and deleting the whole directory is always safe.
//!
//! Two tiers, because the two questions have different resolutions:
//!
//! - **`dt=` — hourly rows for a closed day.** Serves windows up to about
//!   a month, where the console buckets by hours.
//! - **`ym=` — daily rows for a closed month.** Serves windows past that,
//!   where the console buckets by days anyway and hourly detail would be
//!   twenty-four times the reading for none of the answer.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::usage::{RollupRow, fold_rows};

/// Where the caches live, relative to the data directory.
const CACHE_DIR: &str = "rollup-cache";

/// The deltas they are built from.
const SOURCE_DIR: &str = "rollup";

/// Rows for one day at hourly resolution.
///
/// The cache when it is valid; the deltas when it is not. Callers cannot
/// tell the difference apart from how long it took, which is the point.
pub fn hourly_rows(data_dir: &Path, day: &str) -> Vec<RollupRow> {
    let source = data_dir.join(SOURCE_DIR).join(day);
    if let Some(stamp) = fingerprint(&source) {
        if let Some(rows) = read_if_current(&cache_path(data_dir, day), Some(&stamp)) {
            return rows;
        }
        return read_rollup_files(&source);
    }
    // No rollup deltas for this day at all — it predates rollups, or was
    // written by an older node. The backfill turns these into caches on
    // the flusher's thread; until it reaches this one, roll the records
    // up inline.
    //
    // That is slow, and it is still the right answer: the alternative is
    // showing a confident zero for a day that has traffic in it. It also
    // ends — the next maintenance tick caches this day and no reader
    // pays for it again.
    let records = data_dir.join("usage").join(day);
    let stamp = fingerprint(&records).map(|s| format!("records:{s}"));
    if let Some(rows) = read_if_current(&cache_path(data_dir, day), stamp.as_deref()) {
        return rows;
    }
    if stamp.is_none() {
        return Vec::new();
    }
    fold_rows(crate::usage::roll_up(&read_records(&records)))
}

/// Rows for one month at daily resolution.
///
/// Empty when the month has no cache — the caller falls back to walking
/// that month's days, which is what happens for the month in progress.
pub fn monthly_rows(data_dir: &Path, month: &str) -> Option<Vec<RollupRow>> {
    let stamp = month_fingerprint(&data_dir.join(SOURCE_DIR), month);
    read_if_current(&cache_path(data_dir, month), stamp.as_deref())
}

/// Rebuild whatever is missing or stale, skipping anything still being
/// written to.
///
/// Called from the flusher's maintenance tick, on the flusher's own
/// thread — never from a request. A console page load must never be the
/// thing that pays to build a cache; it either finds one or takes the
/// slow path.
pub fn refresh(data_dir: &Path, today: &str, current_month: &str) {
    let source = data_dir.join(SOURCE_DIR);
    let Ok(entries) = std::fs::read_dir(&source) else {
        return;
    };
    let mut days: Vec<String> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            name.starts_with("dt=").then_some(name)
        })
        .collect();
    days.sort();

    for day in &days {
        // The current day is still being appended to; caching it would
        // only mean rebuilding it on the next tick, every tick.
        if day.as_str() >= today {
            continue;
        }
        let Some(stamp) = fingerprint(&source.join(day)) else {
            continue;
        };
        if read_if_current(&cache_path(data_dir, day), Some(&stamp)).is_some() {
            continue;
        }
        let rows = read_rollup_files(&source.join(day));
        if let Err(err) = write_cache(&cache_path(data_dir, day), &stamp, &fold_rows(rows)) {
            tracing::warn!(%err, partition = day, "rollup cache could not be written");
        }
    }

    // Days that hold raw records but no rollups at all: written before
    // rollups existed, or by an older node. `history` used to fall back
    // to scanning their records on every read, which meant the oldest
    // data was also the most expensive. Roll them up once here instead,
    // on this thread, and they join the fast path permanently.
    backfill_missing_rollups(data_dir, today, &days);

    // Months, from the day caches just refreshed. A closed month's days
    // are themselves closed, so this reads one file per day and then
    // never reads them again.
    let mut months: Vec<String> = days
        .iter()
        .filter_map(|day| month_of(day))
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    months.sort();
    for month in months {
        if month.as_str() >= current_month {
            continue;
        }
        let Some(stamp) = month_fingerprint(&source, &month) else {
            continue;
        };
        if read_if_current(&cache_path(data_dir, &month), Some(&stamp)).is_some() {
            continue;
        }
        let rows: Vec<RollupRow> = days
            .iter()
            .filter(|day| month_of(day).as_deref() == Some(month.as_str()))
            .flat_map(|day| to_daily(hourly_rows(data_dir, day)))
            .collect();
        if let Err(err) = write_cache(&cache_path(data_dir, &month), &stamp, &fold_rows(rows)) {
            tracing::warn!(%err, partition = %month, "monthly rollup cache could not be written");
        }
    }
}

/// Build day caches for partitions that have records but no rollups.
///
/// The fingerprint for these comes from the *records* directory, since
/// that is what they are derived from — so a day that later gains rollup
/// deltas invalidates this cache and is rebuilt from the deltas, which
/// is the more authoritative source.
fn backfill_missing_rollups(data_dir: &Path, today: &str, have_rollups: &[String]) {
    let records = data_dir.join("usage");
    let Ok(entries) = std::fs::read_dir(&records) else {
        return;
    };
    for entry in entries.flatten() {
        let day = entry.file_name().to_string_lossy().into_owned();
        if !day.starts_with("dt=") || day.as_str() >= today {
            continue;
        }
        if have_rollups.iter().any(|d| d == &day) {
            continue;
        }
        let Some(stamp) = fingerprint(&records.join(&day)) else {
            continue;
        };
        let stamp = format!("records:{stamp}");
        if read_if_current(&cache_path(data_dir, &day), Some(&stamp)).is_some() {
            continue;
        }
        let rows = crate::usage::roll_up(&read_records(&records.join(&day)));
        if let Err(err) = write_cache(&cache_path(data_dir, &day), &stamp, &fold_rows(rows)) {
            tracing::warn!(%err, partition = %day, "rollup backfill could not be written");
        } else {
            tracing::info!(partition = %day, "rolled up a partition that predates rollups");
        }
    }
}

/// Every raw record in one day partition. Only ever read once per day,
/// by the backfill above.
fn read_records(day_dir: &Path) -> Vec<crate::usage::UsageRecord> {
    let Ok(files) = std::fs::read_dir(day_dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for file in files.flatten() {
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("zst") {
            continue;
        }
        let Ok(handle) = std::fs::File::open(&path) else {
            continue;
        };
        let Ok(decoder) = zstd::Decoder::new(handle) else {
            continue;
        };
        out.extend(
            std::io::BufRead::lines(std::io::BufReader::new(decoder))
                .map_while(Result::ok)
                .filter_map(|line| serde_json::from_str::<crate::usage::UsageRecord>(&line).ok()),
        );
    }
    out
}

/// Drop caches whose source partitions have been pruned away.
///
/// Derived data outliving its source is only wasted disk, but it is also
/// confusing to look at, and a cache for a day that no longer exists can
/// never be validated again.
pub fn prune(data_dir: &Path, cutoff_day: &str) {
    let Ok(entries) = std::fs::read_dir(data_dir.join(CACHE_DIR)) else {
        return;
    };
    let cutoff_month = month_of(cutoff_day);
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(partition) = name.split('.').next() else {
            continue;
        };
        let stale = if partition.starts_with("dt=") {
            partition < cutoff_day
        } else if partition.starts_with("ym=") {
            cutoff_month.as_deref().is_some_and(|m| partition < m)
        } else {
            false
        };
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Fold hourly rows down to one row per day, keyed at midnight.
///
/// Kept as `RollupRow` rather than a new type: everything downstream
/// already knows how to read the day out of `hour_ms`, and a second
/// almost-identical struct would be two things to keep in agreement.
pub fn to_daily(rows: Vec<RollupRow>) -> Vec<RollupRow> {
    fold_rows(rows.into_iter().map(|mut row| {
        row.hour_ms -= row.hour_ms % 86_400_000;
        row
    }))
}

// ---------------------------------------------------------------------------
// Fingerprints
// ---------------------------------------------------------------------------

/// What a day's delta directory looks like right now.
///
/// Names and count, not contents: a delta file is written once and never
/// reopened, so a directory listing that has not changed means the rows
/// have not changed. Reading the files to check whether the cache of
/// those files is current would defeat the entire purpose.
///
/// `None` when the directory does not exist, which is different from an
/// empty one and must not be cached as "no rows".
fn fingerprint(day_dir: &Path) -> Option<String> {
    let entries = std::fs::read_dir(day_dir).ok()?;
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".zst"))
        .collect();
    names.sort();
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for name in &names {
        for byte in name.as_bytes() {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x1000_0000_01b3);
        }
    }
    Some(format!("{}:{hash:016x}", names.len()))
}

/// A month's fingerprint is its days' fingerprints, in order.
fn month_fingerprint(source: &Path, month: &str) -> Option<String> {
    let entries = std::fs::read_dir(source).ok()?;
    let mut days: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|name| month_of(name).as_deref() == Some(month))
        .collect();
    days.sort();
    if days.is_empty() {
        return None;
    }
    let mut parts = Vec::with_capacity(days.len());
    for day in &days {
        parts.push(fingerprint(&source.join(day))?);
    }
    Some(parts.join("|"))
}

/// `dt=2026-08-25` -> `ym=2026-08`.
fn month_of(day: &str) -> Option<String> {
    let date = day.strip_prefix("dt=")?;
    (date.len() >= 7).then(|| format!("ym={}", &date[..7]))
}

// ---------------------------------------------------------------------------
// Files
// ---------------------------------------------------------------------------

fn cache_path(data_dir: &Path, partition: &str) -> PathBuf {
    data_dir
        .join(CACHE_DIR)
        .join(format!("{partition}.jsonl.zst"))
}

fn stamp_path(cache: &Path) -> PathBuf {
    cache.with_extension("zst.stamp")
}

/// The cached rows, if the cache exists and was built from exactly this
/// fingerprint.
fn read_if_current(cache: &Path, want: Option<&str>) -> Option<Vec<RollupRow>> {
    let want = want?;
    let have = std::fs::read_to_string(stamp_path(cache)).ok()?;
    if have.trim() != want {
        return None;
    }
    let handle = std::fs::File::open(cache).ok()?;
    let decoder = zstd::Decoder::new(handle).ok()?;
    Some(
        std::io::BufRead::lines(std::io::BufReader::new(decoder))
            .map_while(Result::ok)
            .filter_map(|line| serde_json::from_str::<RollupRow>(&line).ok())
            .collect(),
    )
}

/// Write the rows, then the stamp.
///
/// That order matters: the stamp is what makes the cache readable, so
/// writing it last means a crash mid-write leaves a cache that is simply
/// ignored rather than one that is trusted and wrong.
fn write_cache(cache: &Path, stamp: &str, rows: &[RollupRow]) -> std::io::Result<()> {
    if let Some(parent) = cache.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temporary = cache.with_extension("zst.building");
    {
        let file = std::fs::File::create(&temporary)?;
        let mut encoder = zstd::Encoder::new(file, 3)?;
        for row in rows {
            serde_json::to_writer(&mut encoder, row)?;
            encoder.write_all(b"\n")?;
        }
        encoder.finish()?.sync_all()?;
    }
    let _ = std::fs::remove_file(stamp_path(cache));
    std::fs::rename(&temporary, cache)?;
    std::fs::write(stamp_path(cache), stamp)?;
    Ok(())
}

/// Every rollup row in one delta directory — the slow path this cache
/// exists to avoid, kept because it is also the path that builds it.
fn read_rollup_files(day_dir: &Path) -> Vec<RollupRow> {
    let Ok(files) = std::fs::read_dir(day_dir) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for file in files.flatten() {
        let path = file.path();
        if path.extension().and_then(|e| e.to_str()) != Some("zst") {
            continue;
        }
        let Ok(handle) = std::fs::File::open(&path) else {
            continue;
        };
        let Ok(decoder) = zstd::Decoder::new(handle) else {
            continue;
        };
        for line in std::io::BufRead::lines(std::io::BufReader::new(decoder)).map_while(Result::ok)
        {
            if let Ok(row) = serde_json::from_str::<RollupRow>(&line) {
                rows.push(row);
            }
        }
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::usage::write_rollups_for_test;

    fn record(ts: u64, model: &str, latency_ms: u64) -> crate::usage::UsageRecord {
        crate::usage::UsageRecord {
            ts,
            request_id: format!("r{ts}{model}"),
            endpoint: "/v1/chat/completions".into(),
            requested: model.into(),
            provider: "openai".into(),
            model: model.into(),
            vkey: None,
            status: 200,
            input_tokens: 10,
            output_tokens: 20,
            cost_micro_usd: 100,
            latency_ms,
            stream: false,
            attempts: 1,
            cached_tokens: 0,
            overhead_us: 0,
            tag: None,
            prompt: None,
            meta: Default::default(),
            error_class: None,
            seat: None,
            ttft_ms: None,
            queue_lag_ms: None,
        }
    }

    /// A day, written as many delta files the way the flusher writes them.
    fn seed_day(dir: &Path, day_start_ms: u64, batches: u64) {
        for seq in 0..batches {
            let batch: Vec<_> = (0..5)
                .map(|i| record(day_start_ms + seq * 1000 + i, "gpt-4o", 100 + i))
                .collect();
            write_rollups_for_test(dir, "n1", seq, &batch);
        }
    }

    /// 2026-08-01T00:00:00Z and 2026-09-01T00:00:00Z.
    const AUG_1: u64 = 1_785_542_400_000;
    const SEP_1: u64 = 1_788_220_800_000;

    #[test]
    fn a_cache_answers_with_exactly_what_the_deltas_hold() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 12);
        let day = crate::usage::day_partition(AUG_1);

        let slow = fold_rows(read_rollup_files(&dir.path().join(SOURCE_DIR).join(&day)));
        refresh(dir.path(), "dt=2026-09-01", "ym=2026-09");
        let fast = hourly_rows(dir.path(), &day);

        assert!(!slow.is_empty(), "the fixture wrote nothing");
        assert_eq!(fold_rows(fast), slow);
    }

    #[test]
    fn a_new_delta_invalidates_the_cache_rather_than_being_missed() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 4);
        let day = crate::usage::day_partition(AUG_1);
        refresh(dir.path(), "dt=2026-09-01", "ym=2026-09");
        let before: u64 = hourly_rows(dir.path(), &day)
            .iter()
            .map(|r| r.requests)
            .sum();

        // A late-arriving batch for a day already cached.
        write_rollups_for_test(dir.path(), "n1", 99, &[record(AUG_1 + 7, "gpt-4o", 100)]);
        let after: u64 = hourly_rows(dir.path(), &day)
            .iter()
            .map(|r| r.requests)
            .sum();

        assert_eq!(after, before + 1, "the stale cache was trusted");
    }

    #[test]
    fn the_current_day_is_never_cached() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 3);
        let day = crate::usage::day_partition(AUG_1);
        // "Today" is the day we just wrote.
        refresh(dir.path(), &day, "ym=2026-08");
        assert!(
            !cache_path(dir.path(), &day).exists(),
            "cached a partition still being written to",
        );
    }

    #[test]
    fn a_month_folds_its_days_into_daily_rows() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 3);
        seed_day(dir.path(), AUG_1 + 86_400_000, 3);
        refresh(dir.path(), "dt=2026-09-02", "ym=2026-09");

        let month = monthly_rows(dir.path(), "ym=2026-08").expect("month cached");
        // Two days of traffic, one row each (one provider, one model).
        assert_eq!(month.len(), 2, "{month:#?}");
        let total: u64 = month.iter().map(|r| r.requests).sum();
        assert_eq!(total, 30);
        for row in &month {
            assert_eq!(
                row.hour_ms % 86_400_000,
                0,
                "a daily row must sit on midnight"
            );
        }
    }

    #[test]
    fn the_month_in_progress_has_no_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 2);
        refresh(dir.path(), "dt=2026-08-30", "ym=2026-08");
        assert!(monthly_rows(dir.path(), "ym=2026-08").is_none());
    }

    #[test]
    fn a_missing_partition_is_not_an_empty_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(hourly_rows(dir.path(), "dt=1999-01-01").is_empty());
        assert!(monthly_rows(dir.path(), "ym=1999-01").is_none());
    }

    #[test]
    fn latency_survives_the_round_trip_through_a_cache() {
        let dir = tempfile::tempdir().expect("tempdir");
        let batch: Vec<_> = (1..=100)
            .map(|i| record(AUG_1 + i, "gpt-4o", i * 10))
            .collect();
        write_rollups_for_test(dir.path(), "n1", 0, &batch);
        let day = crate::usage::day_partition(AUG_1);
        refresh(dir.path(), "dt=2026-09-01", "ym=2026-09");

        let rows = hourly_rows(dir.path(), &day);
        let mut merged = crate::histogram::LatencyHistogram::default();
        for row in &rows {
            merged.merge(&row.latency);
        }
        assert_eq!(merged.count(), 100);
        // True p95 of 10..=1000 by tens is 950.
        assert!(
            (950..=1150).contains(&merged.percentile(95)),
            "{}",
            merged.percentile(95)
        );
    }

    #[test]
    fn pruning_drops_caches_whose_partitions_are_gone() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 2);
        seed_day(dir.path(), SEP_1, 2);
        refresh(dir.path(), "dt=2026-10-01", "ym=2026-10");
        assert!(cache_path(dir.path(), "dt=2026-08-01").exists());

        prune(dir.path(), "dt=2026-09-01");
        assert!(!cache_path(dir.path(), "dt=2026-08-01").exists());
        assert!(!cache_path(dir.path(), "ym=2026-08").exists());
        assert!(cache_path(dir.path(), "dt=2026-09-01").exists());
    }

    #[test]
    fn a_corrupt_stamp_costs_a_slow_read_not_a_wrong_answer() {
        let dir = tempfile::tempdir().expect("tempdir");
        seed_day(dir.path(), AUG_1, 3);
        let day = crate::usage::day_partition(AUG_1);
        refresh(dir.path(), "dt=2026-09-01", "ym=2026-09");
        std::fs::write(stamp_path(&cache_path(dir.path(), &day)), "nonsense").unwrap();

        let rows = hourly_rows(dir.path(), &day);
        let direct = fold_rows(read_rollup_files(&dir.path().join(SOURCE_DIR).join(&day)));
        assert_eq!(fold_rows(rows), direct);
    }
}
