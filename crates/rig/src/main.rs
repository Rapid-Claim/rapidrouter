//! The performance rig: measures what the gateway *adds*, honestly.
//!
//! All modes spawn the mock provider and the gateway in-process, then
//! compare going through the gateway against hitting the mock directly
//! with the same client — the delta is gateway overhead, client costs
//! cancel.
//!
//!   rig overhead --rps 1000 --secs 10 [--assert-p50-us N --assert-p99-us N]
//!   rig stream   --streams 200        [--assert-ttft-delta-us N]
//!   rig soak     --secs 60 --rps 200  [--assert-rss-growth-pct N]
//!
//! Load is open-loop (fixed request schedule) so coordinated omission
//! does not flatter the tail.

use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use hdrhistogram::Histogram;
use router_core::config::{Config, Format};
use router_server::{AppState, build_router};
use serde_json::json;

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(name = "rig", about = "rapid-router performance rig")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fixed-RPS request latency: gateway vs direct, p50/p99/p999 delta.
    Overhead {
        #[arg(long, default_value_t = 500)]
        rps: u64,
        #[arg(long, default_value_t = 10)]
        secs: u64,
        #[arg(long)]
        assert_p50_us: Option<u64>,
        #[arg(long)]
        assert_p99_us: Option<u64>,
    },
    /// Streaming: time-to-first-byte delta and whole-stream duration.
    Stream {
        #[arg(long, default_value_t = 100)]
        streams: u64,
        #[arg(long)]
        assert_ttft_delta_us: Option<u64>,
    },
    /// Sustained load with RSS sampling; asserts flat memory.
    Soak {
        #[arg(long, default_value_t = 60)]
        secs: u64,
        #[arg(long, default_value_t = 200)]
        rps: u64,
        #[arg(long, default_value_t = 25)]
        assert_rss_growth_pct: u64,
    },
}

struct Bench {
    gateway_url: String,
    direct_url: String,
}

async fn spawn_stack() -> Bench {
    let mock = mock_provider::MockProvider::spawn().await;
    let config = Config::from_str_with_env(
        &format!(
            "[providers.openai]\nbase_url = \"{}\"\nkeys = [{{ name = \"k\", value = \"sk-rig\" }}]\nmax_concurrency = 65535\n",
            mock.base_url()
        ),
        Format::Toml,
        &|_: &str| None,
    )
    .unwrap();
    let state = AppState::new(config);
    let app = build_router(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let gateway_url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        router_server::serve(listener, state, app, std::future::pending())
            .await
            .unwrap()
    });
    Bench {
        gateway_url: format!("{gateway_url}/v1/chat/completions"),
        direct_url: format!("{}/chat/completions", mock.base_url()),
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(256)
        .tcp_nodelay(true)
        .build()
        .unwrap()
}

fn body() -> serde_json::Value {
    json!({
        "model": "openai/gpt-4o",
        "messages": [
            {"role": "system", "content": "be brief"},
            {"role": "user", "content": "hello there, how are the things going today?"}
        ],
        "temperature": 0.7
    })
}

/// Open-loop run against one URL. Returns (round-trip µs, gateway
/// self-reported x-rapid-overhead-us) histograms; the second is empty
/// for direct runs.
async fn run_fixed_rps(url: &str, rps: u64, secs: u64) -> (Histogram<u64>, Histogram<u64>) {
    let client = client();
    // Warm the connection pool.
    for _ in 0..32 {
        let _ = client.post(url).json(&body()).send().await.unwrap();
    }

    let total = rps * secs;
    let interval = Duration::from_nanos(1_000_000_000 / rps);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<(u64, Option<u64>)>();
    let url = url.to_owned();
    let client = Arc::new(client);

    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
    for _ in 0..total {
        ticker.tick().await;
        let client = client.clone();
        let url = url.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let res = client.post(&url).json(&body()).send().await.unwrap();
            let internal = res
                .headers()
                .get("x-rapid-overhead-us")
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok());
            let _ = res.bytes().await.unwrap();
            let _ = tx.send((start.elapsed().as_micros() as u64, internal));
        });
    }
    drop(tx);

    let mut round_trip = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    let mut internal = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    while let Some((us, gw)) = rx.recv().await {
        round_trip.record(us.max(1)).unwrap();
        if let Some(gw) = gw {
            internal.record(gw.max(1)).unwrap();
        }
    }
    (round_trip, internal)
}

fn print_comparison(name: &str, direct: &Histogram<u64>, gateway: &Histogram<u64>) -> (i64, i64) {
    let p = |h: &Histogram<u64>, q: f64| h.value_at_quantile(q) as i64;
    let d50 = p(gateway, 0.50) - p(direct, 0.50);
    let d99 = p(gateway, 0.99) - p(direct, 0.99);
    let d999 = p(gateway, 0.999) - p(direct, 0.999);
    println!("\n{name} (latencies in µs)");
    println!("{:<10} {:>10} {:>10} {:>10}", "", "p50", "p99", "p99.9");
    println!(
        "{:<10} {:>10} {:>10} {:>10}",
        "direct",
        p(direct, 0.5),
        p(direct, 0.99),
        p(direct, 0.999)
    );
    println!(
        "{:<10} {:>10} {:>10} {:>10}",
        "gateway",
        p(gateway, 0.5),
        p(gateway, 0.99),
        p(gateway, 0.999)
    );
    println!("{:<10} {:>10} {:>10} {:>10}", "overhead", d50, d99, d999);
    (d50, d99)
}

async fn cmd_overhead(rps: u64, secs: u64, assert_p50: Option<u64>, assert_p99: Option<u64>) {
    let bench = spawn_stack().await;
    println!("overhead rig: {rps} rps x {secs}s, open loop, warm pools");
    let (direct, _) = run_fixed_rps(&bench.direct_url, rps, secs).await;
    let (gateway, internal) = run_fixed_rps(&bench.gateway_url, rps, secs).await;
    let (d50, d99) = print_comparison("sync completion round-trip", &direct, &gateway);
    println!(
        "\ngateway-internal overhead (x-rapid-overhead-us): p50 {}µs  p99 {}µs  p99.9 {}µs",
        internal.value_at_quantile(0.5),
        internal.value_at_quantile(0.99),
        internal.value_at_quantile(0.999),
    );
    println!(
        "(round-trip delta includes one extra localhost TCP hop; the header is the gateway's own added time)"
    );

    let mut failed = false;
    if let Some(limit) = assert_p50
        && d50 > limit as i64
    {
        eprintln!("ASSERT FAILED: p50 overhead {d50}µs > {limit}µs");
        failed = true;
    }
    if let Some(limit) = assert_p99
        && d99 > limit as i64
    {
        eprintln!("ASSERT FAILED: p99 overhead {d99}µs > {limit}µs");
        failed = true;
    }
    if failed {
        std::process::exit(1);
    }
}

/// TTFB of a streaming request, plus whole-stream duration.
async fn stream_once(client: &reqwest::Client, url: &str) -> (u64, u64) {
    use futures_util::StreamExt;
    let mut b = body();
    b["stream"] = json!(true);
    let start = Instant::now();
    let res = client.post(url).json(&b).send().await.unwrap();
    let mut stream = res.bytes_stream();
    let mut ttfb = None;
    while let Some(chunk) = stream.next().await {
        let _ = chunk.unwrap();
        ttfb.get_or_insert_with(|| start.elapsed().as_micros() as u64);
    }
    (ttfb.unwrap_or_default(), start.elapsed().as_micros() as u64)
}

async fn cmd_stream(streams: u64, assert_ttft: Option<u64>) {
    let bench = spawn_stack().await;
    let client = client();
    let mut direct_ttfb = Histogram::<u64>::new(3).unwrap();
    let mut gateway_ttfb = Histogram::<u64>::new(3).unwrap();
    for _ in 0..streams {
        let (t, _) = stream_once(&client, &bench.direct_url).await;
        direct_ttfb.record(t).unwrap();
        let (t, _) = stream_once(&client, &bench.gateway_url).await;
        gateway_ttfb.record(t).unwrap();
    }
    let (d50, _) = print_comparison("streaming TTFB", &direct_ttfb, &gateway_ttfb);
    if let Some(limit) = assert_ttft
        && d50 > limit as i64
    {
        eprintln!("ASSERT FAILED: TTFB p50 delta {d50}µs > {limit}µs");
        std::process::exit(1);
    }
}

fn rss_kb() -> u64 {
    let pid = std::process::id().to_string();
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .unwrap_or(0)
}

async fn cmd_soak(secs: u64, rps: u64, assert_growth_pct: u64) {
    let bench = spawn_stack().await;
    println!("soak: {rps} rps x {secs}s with RSS sampling");
    // Warm-up establishes steady state before the baseline sample.
    let _ = run_fixed_rps(&bench.gateway_url, rps, 3).await;
    #[allow(clippy::let_underscore_future)]
    let baseline = rss_kb();
    println!("baseline RSS: {baseline} KB");

    let chunks = (secs / 10).max(1);
    let mut samples = vec![baseline];
    for i in 0..chunks {
        let (histogram, _) = run_fixed_rps(&bench.gateway_url, rps, 10.min(secs)).await;
        let rss = rss_kb();
        samples.push(rss);
        println!(
            "t+{:>4}s RSS {rss} KB, p99 {}µs",
            (i + 1) * 10,
            histogram.value_at_quantile(0.99)
        );
    }
    // RSS under load is a sawtooth: the allocator holds and releases
    // arenas, so two instantaneous samples can differ by 2x with no leak
    // whatsoever. "Flat memory" means the trailing steady state is no
    // higher than the leading one, so compare medians of the first and
    // last thirds rather than first-vs-last samples.
    let third = (samples.len() / 3).max(1);
    let early = median(&samples[..third]);
    let late = median(&samples[samples.len() - third..]);
    let peak = samples.iter().copied().max().unwrap_or(0);
    let growth_pct = late.saturating_sub(early) * 100 / early.max(1);
    println!(
        "RSS: early-median {early} KB, late-median {late} KB, peak {peak} KB -> growth {growth_pct}%"
    );
    if growth_pct > assert_growth_pct {
        eprintln!("ASSERT FAILED: RSS grew {growth_pct}% > {assert_growth_pct}%");
        std::process::exit(1);
    }
}

fn median(values: &[u64]) -> u64 {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted[sorted.len() / 2]
}

fn main() {
    let cli = Cli::parse();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        match cli.command {
            Command::Overhead {
                rps,
                secs,
                assert_p50_us,
                assert_p99_us,
            } => cmd_overhead(rps, secs, assert_p50_us, assert_p99_us).await,
            Command::Stream {
                streams,
                assert_ttft_delta_us,
            } => cmd_stream(streams, assert_ttft_delta_us).await,
            Command::Soak {
                secs,
                rps,
                assert_rss_growth_pct,
            } => cmd_soak(secs, rps, assert_rss_growth_pct).await,
        }
    });
}
