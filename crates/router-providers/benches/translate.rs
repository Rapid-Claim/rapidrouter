//! Translation-stage benchmarks: the cross-dialect cost per request and
//! per stream event.

use criterion::{Criterion, black_box, criterion_group, criterion_main};
use router_core::chat::ChatRequest;
use router_core::sse::SseEvent;
use serde_json::json;

fn internal_request() -> ChatRequest {
    serde_json::from_value(json!({
        "model": "m",
        "messages": [
            {"role": "system", "content": "be precise"},
            {"role": "user", "content": "compare the weather in these two cities please"},
            {"role": "assistant", "tool_calls": [
                {"id": "call_a", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"Paris\"}"}}]},
            {"role": "tool", "tool_call_id": "call_a", "content": "{\"temp\": 21}"},
            {"role": "user", "content": "and now?"}
        ],
        "tools": [{"type": "function", "function": {"name": "get_weather",
            "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}}],
        "temperature": 0.5, "max_tokens": 200
    }))
    .unwrap()
}

fn bench_requests(c: &mut Criterion) {
    let req = internal_request();
    c.bench_function("build_anthropic_request", |b| {
        b.iter(|| router_providers::anthropic::build_request(black_box(&req), "claude-x").unwrap())
    });
    c.bench_function("build_gemini_request", |b| {
        b.iter(|| router_providers::gemini::build_request(black_box(&req)).unwrap())
    });
}

fn bench_stream_event(c: &mut Criterion) {
    let event = SseEvent {
        event: Some("content_block_delta".into()),
        data: r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hello world, more text here"}}"#.into(),
    };
    c.bench_function("anthropic_stream_event_to_chunk", |b| {
        let mut state = router_providers::anthropic::StreamToOpenAi::new(false);
        b.iter(|| state.on_event(black_box(&event)))
    });
}

criterion_group!(benches, bench_requests, bench_stream_event);
criterion_main!(benches);
