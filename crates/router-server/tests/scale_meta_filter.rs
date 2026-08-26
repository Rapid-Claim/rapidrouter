//! What a caller-dimension filter costs at scale.
//!
//! Filtering on a dimension is a scan: records carry the dimensions and
//! nothing indexes them, so the honest question is not "is it fast" but
//! "what does it cost, and where does it stop being acceptable". This
//! writes a day of traffic and times the shapes that differ — a common
//! value, a rare one, and one that matches nothing at all.
//!
//! `#[ignore]` because it writes hundreds of megabytes; run it
//! deliberately with `--ignored`.

use std::collections::BTreeMap;

use router_server::usage::{HistoryFilter, UsagePipeline, UsageRecord};

/// Dimensions shaped like the ones a real fleet sends: a handful of
/// workflows, a few services, and an unbounded chart id.
fn record(i: usize, ts: u64) -> UsageRecord {
    let workflows = [
        "WORKFLOW_HCC_CONFIRMED",
        "WORKFLOW_HCC_SUSPECTED",
        "WORKFLOW_RISE_HCS",
        "WORKFLOW_RISE_SILVER",
        "WORKFLOW_HOM_ECW",
    ];
    let stages = [
        "ICD_EXTRACTION",
        "ICD_SEARCH",
        "CPT_SEARCH",
        "CHUNKER",
        "DIRECT_CODING",
        "GUIDELINES",
    ];
    let services = ["coding-orchestrator", "chart_parser", "agentic_dag_coder"];
    let mut meta = BTreeMap::new();
    meta.insert("workflow_id".into(), workflows[i % workflows.len()].into());
    meta.insert("stage".into(), stages[i % stages.len()].into());
    meta.insert("service".into(), services[i % services.len()].into());
    meta.insert("agent".into(), format!("agent_{}", i % 40));
    // Unbounded, the way a chart id is: one request in a hundred thousand
    // shares a value with any other.
    meta.insert(
        "chart_id".into(),
        format!("{}", 20_250_000_000_000u64 + i as u64),
    );
    meta.insert("org_id".into(), format!("org-{}", i % 12));
    UsageRecord {
        ts,
        request_id: format!("req-{i}"),
        endpoint: "chat".into(),
        requested: "openai/gpt-4o-mini".into(),
        provider: "openai".into(),
        model: "gpt-4o-mini".into(),
        vkey: Some(format!("vk-{}", i % 5)),
        status: if i.is_multiple_of(50) { 429 } else { 200 },
        stream: i.is_multiple_of(3),
        input_tokens: 800,
        output_tokens: 150,
        cached_tokens: 0,
        cost_micro_usd: 2_500,
        latency_ms: 400,
        overhead_us: 120,
        attempts: 1,
        tag: None,
        prompt: Some("a seeded request".into()),
        meta,
        error_class: None,
        seat: Some("openai/primary".into()),
        ttft_ms: Some(80),
        queue_lag_ms: Some(1_200),
    }
}

#[test]
#[ignore = "writes a day of synthetic traffic; run explicitly"]
fn a_dimension_filter_over_a_day_stays_bounded() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let day_ms = 86_400_000u64;
    let total = 1_000_000usize;
    let batch_size = 10_000usize;

    let started = std::time::Instant::now();
    for batch in 0..(total / batch_size) {
        let records: Vec<UsageRecord> = (0..batch_size)
            .map(|k| {
                let i = batch * batch_size + k;
                // Newest last, so file order is time order.
                record(i, now - day_ms + (i as u64 * day_ms / total as u64))
            })
            .collect();
        router_server::usage::write_batch_for_test(&root, "node-a", batch as u64, &records);
    }
    eprintln!("wrote {total} records in {:?}", started.elapsed());

    let pipeline = UsagePipeline::for_test(Some(root));
    let page = 100usize;

    let time = |name: &str, filter: HistoryFilter| {
        let t = std::time::Instant::now();
        let rows = pipeline.recent_from_disk(page, now - day_ms, now, &filter, false);
        eprintln!("  {name:<46} {:>4} rows in {:?}", rows.len(), t.elapsed());
        (rows.len(), t.elapsed())
    };

    let meta = |pairs: &[(&str, &str)]| HistoryFilter {
        meta: pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
        ..Default::default()
    };

    eprintln!("\nfirst page of 100, newest first, over a day of 1M records:");
    let (_, unfiltered) = time("no filter (baseline)", HistoryFilter::default());
    let (_, common) = time(
        "meta.workflow_id=… (1 in 5)",
        meta(&[("workflow_id", "WORKFLOW_RISE_HCS")]),
    );
    let (_, two) = time(
        "workflow_id + stage (1 in 30)",
        meta(&[
            ("workflow_id", "WORKFLOW_RISE_HCS"),
            ("stage", "CPT_SEARCH"),
        ]),
    );
    let (_, rare) = time("meta.agent=… (1 in 40)", meta(&[("agent", "agent_7")]));
    let (chart_rows, chart) = time(
        "meta.chart_id=… (1 in 1,000,000 — worst case)",
        meta(&[("chart_id", "20250000500000")]),
    );
    let (none_rows, unmatched) = time(
        "a value nothing carries (full scan, 0 rows)",
        meta(&[("workflow_id", "WORKFLOW_NOPE")]),
    );

    assert!(none_rows == 0, "an unmatched term must return nothing");
    assert!(chart_rows <= 1, "a chart id is unique in this fixture");

    // The point of the assertions: a selective filter degrades to a full
    // scan, and that is the cost worth knowing rather than hiding.
    eprintln!(
        "\nselective filter costs {:.1}x the unfiltered first page",
        chart.as_secs_f64() / unfiltered.as_secs_f64().max(1e-9)
    );
    for (name, taken) in [
        ("unfiltered", unfiltered),
        ("common value", common),
        ("two terms", two),
        ("rare value", rare),
        ("worst case", chart),
        ("unmatched", unmatched),
    ] {
        assert!(
            taken < std::time::Duration::from_secs(30),
            "{name} took {taken:?}; a console read must not hang"
        );
    }
}
