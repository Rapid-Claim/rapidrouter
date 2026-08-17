use router_server::usage::{HistoryFilter, UsagePipeline, UsageRecord, write_batch_for_test, write_rollups_for_test};

fn record(ts: u64, provider: &str, model: &str, vkey: Option<&str>) -> UsageRecord {
    UsageRecord {
        ts,
        request_id: format!("r{ts}"),
        endpoint: "/v1/chat/completions".into(),
        requested: model.into(),
        provider: provider.into(),
        model: model.into(),
        vkey: vkey.map(str::to_owned),
        status: 200,
        stream: false,
        input_tokens: 100,
        output_tokens: 20,
        cached_tokens: 0,
        cost_micro_usd: 1_000,
        latency_ms: 250,
        overhead_us: 10,
        attempts: 1,
        tag: None,
    }
}

#[test]
fn seven_days_of_history_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let now = router_core::vkey::unix_now_ms();
    // One record per day for the last 7 days.
    for d in 0..7u64 {
        let ts = now - d * 86_400_000;
        let batch = vec![record(ts, "openai", "gpt-5", Some("vk_a"))];
        write_batch_for_test(&root, "node-a", d, &batch);
        write_rollups_for_test(&root, "node-a", d, &batch);
    }
    let p = UsagePipeline::for_test(Some(root));
    for days in [1u32, 2, 8, 31] {
        let out = p.history(days, "model", &HistoryFilter::default());
        let total: u64 = out.values().flatten().map(|b| b.requests).sum();
        println!("days={days} series={:?} total={total}", out.keys().collect::<Vec<_>>());
        for (k, v) in &out {
            println!("   {k}: {:?}", v.iter().map(|b| (&b.day, b.requests)).collect::<Vec<_>>());
        }
    }
    let f = HistoryFilter { provider: Some("openai".into()), model: None, vkey: None };
    let out = p.history(8, "model", &f);
    println!("filtered provider=openai: {:?}", out.iter().map(|(k,v)| (k, v.len())).collect::<Vec<_>>());
    let f = HistoryFilter { provider: None, model: None, vkey: Some("vk_a".into()) };
    let out = p.history(8, "model", &f);
    println!("filtered key=vk_a: {:?}", out.iter().map(|(k,v)| (k, v.len())).collect::<Vec<_>>());
}
