//! History at the scale this gateway is aimed at.
//!
//! One million requests a day is the number that motivated rollups; this
//! writes a week of it and asserts the console's read stays bounded. It
//! is `#[ignore]` because it writes ~1.5 GB and takes minutes — run it
//! deliberately with `--ignored`, not on every commit.

use router_server::usage::{HistoryFilter, UsagePipeline};

#[test]
#[ignore = "writes a week of synthetic traffic; run explicitly"]
fn a_week_of_a_million_requests_a_day_reads_fast() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();

    let providers = ["openai", "anthropic", "gemini"];
    let models = [
        "gpt-5",
        "gpt-5-mini",
        "claude-sonnet-4-5",
        "claude-opus-4-1",
        "gemini-2.5-pro",
        "gemini-2.5-flash",
        "gpt-4o",
        "o3",
    ];
    let keys = ["vk_app", "vk_batch", "vk_eval", "vk_internal", "vk_public"];

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let day_ms = 86_400_000u64;
    let per_day = 1_000_000usize;
    let mut written = 0usize;

    let started = std::time::Instant::now();
    for day in 0..7u64 {
        let day_start = now - (day + 1) * day_ms;
        // The flusher writes in batches; mirror that shape.
        for batch_idx in 0..(per_day / 10_000) {
            let batch: Vec<_> = (0..10_000)
                .map(|i| {
                    let n = batch_idx * 10_000 + i;
                    let ts = day_start + (n as u64 * day_ms / per_day as u64);
                    router_server::usage::UsageRecord {
                        ts,
                        request_id: format!("req_{day}_{n}"),
                        endpoint: "/v1/chat/completions".into(),
                        requested: models[n % models.len()].into(),
                        provider: providers[n % providers.len()].into(),
                        model: models[n % models.len()].into(),
                        vkey: Some(keys[n % keys.len()].into()),
                        status: if n % 50 == 0 { 429 } else { 200 },
                        stream: n % 3 == 0,
                        input_tokens: 800 + (n % 400) as u64,
                        output_tokens: 150 + (n % 100) as u64,
                        cached_tokens: 0,
                        cost_micro_usd: 2_500 + (n % 900) as u64,
                        latency_ms: 400 + (n % 2_000) as u64,
                        overhead_us: 120,
                        attempts: 1,
                        tag: None,
                        prompt: None,
                        meta: Default::default(),
                        error_class: None,
                        seat: None,
                        ttft_ms: None,
                        queue_lag_ms: None,
                    }
                })
                .collect();
            router_server::usage::write_batch_for_test(
                &root,
                "node-a",
                day * 1000 + batch_idx as u64,
                &batch,
            );
            router_server::usage::write_rollups_for_test(
                &root,
                "node-a",
                day * 1000 + batch_idx as u64,
                &batch,
            );
            written += batch.len();
        }
    }
    eprintln!("wrote {written} records in {:?}", started.elapsed());

    let pipeline = UsagePipeline::for_test(Some(root.clone()));

    let t0 = std::time::Instant::now();
    let history = pipeline.history(7, "model", &HistoryFilter::default());
    let rollup_time = t0.elapsed();
    let total: u64 = history
        .values()
        .flat_map(|d| d.iter().map(|b| b.requests))
        .sum();
    eprintln!(
        "rollup read: {rollup_time:?} for {total} requests across {} series",
        history.len()
    );

    assert_eq!(
        total as usize, written,
        "the rollup must account for every record"
    );
    // Generous enough to run in a debug build (where it takes ~5×) and
    // still catch a regression that puts a raw scan back on this path.
    assert!(
        rollup_time < std::time::Duration::from_millis(1_500),
        "a week of history must read in well under a second, took {rollup_time:?}",
    );

    // The same question answered the old way, for the record: this is
    // what every chart cost before rollups existed, and what the
    // fallback still costs on a day written by an older node.
    std::fs::remove_dir_all(root.join("rollup")).unwrap();
    let t_raw = std::time::Instant::now();
    let raw = pipeline.history(7, "model", &HistoryFilter::default());
    let raw_time = t_raw.elapsed();
    let raw_total: u64 = raw
        .values()
        .flat_map(|d| d.iter().map(|b| b.requests))
        .sum();
    eprintln!("raw scan:    {raw_time:?} for {raw_total} requests");
    assert_eq!(raw_total, total, "the fallback must agree with the rollup");

    // A filtered log search over the same corpus.
    let t1 = std::time::Instant::now();
    let logs = pipeline.recent_from_disk(
        200,
        now - 2 * day_ms,
        now,
        &HistoryFilter {
            provider: Some("openai".into()),
            ..Default::default()
        },
        false,
    );
    let log_time = t1.elapsed();
    eprintln!("log search: {log_time:?} for {} records", logs.len());
    assert_eq!(logs.len(), 200, "the search must fill the page");
    assert!(
        log_time < std::time::Duration::from_secs(3),
        "a filtered log search must not walk the corpus, took {log_time:?}",
    );
}
