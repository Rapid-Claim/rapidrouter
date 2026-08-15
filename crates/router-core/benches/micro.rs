//! Per-stage micro benchmarks. CI runs these against stored baselines;
//! the latency budgets in the docs are enforced here first.

use bytes::Bytes;
use criterion::{Criterion, black_box, criterion_group, criterion_main};
use router_core::breaker::{Breaker, BreakerConfig};
use router_core::config::{Config, Format};
use router_core::router::RoutingTable;
use router_core::sse::SseParser;
use router_core::{ErrorClass, GatewayError, json};

fn chat_body(content_bytes: usize) -> Bytes {
    let filler = "x".repeat(content_bytes);
    Bytes::from(format!(
        r#"{{"model": "openai/gpt-4o", "stream": true, "messages": [{{"role": "system", "content": "be brief"}}, {{"role": "user", "content": "{filler}"}}], "temperature": 0.7}}"#
    ))
}

fn bench_probe(c: &mut Criterion) {
    let mut group = c.benchmark_group("json_probe");
    for size in [2_000usize, 64_000, 1_000_000] {
        let body = chat_body(size);
        group.throughput(criterion::Throughput::Bytes(body.len() as u64));
        group.bench_function(format!("{size}B"), |b| {
            b.iter(|| json::probe(black_box(&body)).unwrap())
        });
        // The slow path this replaces, for the ratio.
        group.bench_function(format!("{size}B_serde_full_parse"), |b| {
            b.iter(|| serde_json::from_slice::<serde_json::Value>(black_box(&body)).unwrap())
        });
    }
    group.finish();
}

fn bench_splice(c: &mut Criterion) {
    let body = chat_body(2_000);
    let probe = json::probe(&body).unwrap();
    c.bench_function("splice_model_2KB", |b| {
        b.iter(|| json::splice_model(black_box(&body), probe.model_span, black_box("gpt-4o")))
    });
}

fn bench_sse(c: &mut Criterion) {
    let frame = b"data: {\"id\":\"c\",\"choices\":[{\"delta\":{\"content\":\"hello world\"},\"index\":0}]}\n\n";
    c.bench_function("sse_parse_frame", |b| {
        b.iter(|| {
            let mut parser = SseParser::new();
            black_box(parser.push(black_box(frame)))
        })
    });
}

fn bench_routing(c: &mut Criterion) {
    let config = Config::from_str_with_env(
        r#"
[providers.openai]
keys = [
  { name = "a", value = "sk-a", weight = 0.5 },
  { name = "b", value = "sk-b", weight = 0.3 },
  { name = "c", value = "sk-c", weight = 0.2 },
]
[aliases]
fast = "openai/gpt-4o"
"#,
        Format::Toml,
        &|_: &str| None,
    )
    .unwrap();
    let table = RoutingTable::from_config(&config);
    c.bench_function("resolve_alias", |b| {
        b.iter(|| table.resolve(black_box("fast")).unwrap())
    });
    c.bench_function("resolve_prefix", |b| {
        b.iter(|| table.resolve(black_box("openai/gpt-4o-mini")).unwrap())
    });
    let route = table.resolve("openai/gpt-4o").unwrap();
    c.bench_function("admit_key", |b| {
        b.iter(|| route.provider.admit_key(black_box("gpt-4o"), 1000).unwrap())
    });
}

fn bench_breaker(c: &mut Criterion) {
    let breaker = Breaker::new(BreakerConfig {
        failure_threshold: 5,
        window_ms: 30_000,
        cooldown_ms: 15_000,
    });
    c.bench_function("breaker_admit_closed", |b| {
        b.iter(|| breaker.admit(black_box(1000)))
    });
}

fn bench_error_render(c: &mut Criterion) {
    let err = GatewayError::new(ErrorClass::NotFound, "unknown model `x`").with_param("model");
    c.bench_function("error_to_openai_body", |b| {
        b.iter(|| black_box(&err).to_openai_body())
    });
}

criterion_group!(
    benches,
    bench_probe,
    bench_splice,
    bench_sse,
    bench_routing,
    bench_breaker,
    bench_error_render
);
criterion_main!(benches);
