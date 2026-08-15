//! The usage pipeline: token/cost extraction from upstream responses, the
//! hot-path record ring, in-memory aggregation windows, and durable
//! date-partitioned JSONL on disk.
//!
//! The hot path does one bounded channel send; a dedicated flusher thread
//! batches records into `usage/dt=YYYY-MM-DD/*.jsonl.zst` and prunes
//! partitions past retention. Budgets are enforced from the in-memory
//! spend counters, seeded from disk at boot — cutoff lag is bounded by the
//! flush interval, the documented cost of "no database."

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http_body::Frame;
use router_core::config::{Config, Price, UsageConfig};
use router_core::vkey::{self, VkRuntime};
use router_providers::Dialect;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ---------------------------------------------------------------------------
// Token extraction
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    /// Provider-cache reads, excluded from tpm accounting.
    pub cached: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input + self.output
    }

    /// Tokens counted against a key's tpm limit.
    pub fn billable(&self) -> u64 {
        self.input.saturating_sub(self.cached) + self.output
    }

    pub fn is_empty(&self) -> bool {
        self.input == 0 && self.output == 0
    }

    fn merge_max(&mut self, other: TokenUsage) {
        // Streams may repeat usage cumulatively; the largest value wins.
        self.input = self.input.max(other.input);
        self.output = self.output.max(other.output);
        self.cached = self.cached.max(other.cached);
    }
}

fn u(v: &Value) -> u64 {
    v.as_u64().unwrap_or(0)
}

/// Extract usage from a complete (non-streaming) upstream response body in
/// the provider's native dialect.
pub fn extract_sync(dialect: Dialect, body: &[u8]) -> TokenUsage {
    if memchr::memmem::find(body, b"usage").is_none()
        && memchr::memmem::find(body, b"usageMetadata").is_none()
    {
        return TokenUsage::default();
    }
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return TokenUsage::default();
    };
    usage_from_value(dialect, &v)
}

fn usage_from_value(dialect: Dialect, v: &Value) -> TokenUsage {
    match dialect {
        // Codex answers in the Responses shape, which the OpenAI arm
        // already reads from both the flat and the nested position.
        Dialect::OpenAi | Dialect::CodexResponses => {
            // Chat completions (prompt/completion), Responses (input/output),
            // and nested `response.usage` from stream-completed events.
            let usage = if v["usage"].is_object() {
                &v["usage"]
            } else {
                &v["response"]["usage"]
            };
            TokenUsage {
                input: u(&usage["prompt_tokens"]).max(u(&usage["input_tokens"])),
                output: u(&usage["completion_tokens"]).max(u(&usage["output_tokens"])),
                cached: u(&usage["prompt_tokens_details"]["cached_tokens"])
                    .max(u(&usage["input_tokens_details"]["cached_tokens"])),
            }
        }
        Dialect::Anthropic => {
            // Sync bodies and `message_start`/`message_delta` events all
            // carry a `usage` object (possibly nested under `message`).
            let usage = if v["usage"].is_object() {
                &v["usage"]
            } else {
                &v["message"]["usage"]
            };
            TokenUsage {
                input: u(&usage["input_tokens"]),
                output: u(&usage["output_tokens"]),
                cached: u(&usage["cache_read_input_tokens"]),
            }
        }
        Dialect::Gemini => TokenUsage {
            input: u(&v["usageMetadata"]["promptTokenCount"]),
            output: u(&v["usageMetadata"]["candidatesTokenCount"]),
            cached: u(&v["usageMetadata"]["cachedContentTokenCount"]),
        },
        Dialect::Bedrock => TokenUsage {
            input: u(&v["usage"]["inputTokens"]),
            output: u(&v["usage"]["outputTokens"]),
            cached: u(&v["usage"]["cacheReadInputTokens"]),
        },
    }
}

/// Incremental usage scanner for streams: feed every upstream event's JSON
/// payload; cheap marker checks skip events that cannot carry usage.
#[derive(Debug)]
pub struct StreamUsageScanner {
    dialect: Dialect,
    usage: TokenUsage,
}

impl StreamUsageScanner {
    pub fn new(dialect: Dialect) -> Self {
        Self {
            dialect,
            usage: TokenUsage::default(),
        }
    }

    pub fn on_event_data(&mut self, data: &str) {
        if !data.contains("usage") && !data.contains("usageMetadata") {
            return;
        }
        let Ok(v) = serde_json::from_str::<Value>(data) else {
            return;
        };
        self.usage.merge_max(usage_from_value(self.dialect, &v));
    }

    pub fn finish(&self) -> TokenUsage {
        self.usage
    }
}

// ---------------------------------------------------------------------------
// Pricing
// ---------------------------------------------------------------------------

/// Built-in prices, USD per million tokens, matched by substring of the
/// upstream model id (longest match wins). Config `[pricing]` entries
/// override by exact `provider/model`.
const BUILTIN_PRICES: &[(&str, f64, f64)] = &[
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4.1-nano", 0.10, 0.40),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1", 2.00, 8.00),
    ("o4-mini", 1.10, 4.40),
    ("claude-haiku-4-5", 1.00, 5.00),
    ("claude-3-5-haiku", 0.80, 4.00),
    ("claude-sonnet", 3.00, 15.00),
    ("claude-opus", 5.00, 25.00),
    ("gemini-2.5-pro", 1.25, 10.00),
    ("gemini-2.5-flash-lite", 0.10, 0.40),
    ("gemini-2.5-flash", 0.30, 2.50),
    ("gemini-flash-lite", 0.10, 0.40),
    ("gemini-flash", 0.30, 2.50),
    ("gemini-pro", 1.25, 10.00),
];

#[derive(Debug, Default, Clone)]
pub struct Pricing {
    overrides: BTreeMap<String, Price>,
}

impl Pricing {
    pub fn from_config(config: &Config) -> Self {
        Self {
            overrides: config.pricing.clone(),
        }
    }

    pub fn price_for(&self, provider: &str, model: &str) -> Option<Price> {
        if let Some(p) = self.overrides.get(&format!("{provider}/{model}")) {
            return Some(*p);
        }
        BUILTIN_PRICES
            .iter()
            .filter(|(needle, _, _)| model.contains(needle))
            .max_by_key(|(needle, _, _)| needle.len())
            .map(|(_, input, output)| Price {
                input_per_mtok: *input,
                output_per_mtok: *output,
            })
    }

    /// Cost in micro-USD: `tokens × usd-per-mtok` is exactly µUSD.
    pub fn cost_micro_usd(&self, provider: &str, model: &str, usage: TokenUsage) -> u64 {
        let Some(price) = self.price_for(provider, model) else {
            return 0;
        };
        let micro =
            usage.input as f64 * price.input_per_mtok + usage.output as f64 * price.output_per_mtok;
        micro.max(0.0).round() as u64
    }
}

// ---------------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    /// Unix milliseconds.
    pub ts: u64,
    pub request_id: String,
    pub endpoint: String,
    /// The model string the client asked for (alias or provider/model).
    pub requested: String,
    pub provider: String,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vkey: Option<String>,
    pub status: u16,
    pub stream: bool,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micro_usd: u64,
    pub latency_ms: u64,
    pub overhead_us: u64,
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

// ---------------------------------------------------------------------------
// Aggregation (minute buckets, bounded cardinality)
// ---------------------------------------------------------------------------

const MINUTES: usize = 24 * 60;
const MAX_KEYS_PER_MINUTE: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AggKey {
    provider: String,
    model: String,
    vkey: Option<String>,
}

#[derive(Debug, Clone, Copy, Default)]
struct AggCell {
    requests: u64,
    errors: u64,
    input_tokens: u64,
    output_tokens: u64,
    cost_micro_usd: u64,
    latency_ms_sum: u64,
}

#[derive(Default)]
struct MinuteSlot {
    /// Which epoch-minute this slot currently holds; slots recycle lazily.
    minute: u64,
    cells: HashMap<AggKey, AggCell>,
}

pub struct Aggregator {
    slots: Vec<Mutex<MinuteSlot>>,
}

impl Aggregator {
    fn new() -> Self {
        Self {
            slots: (0..MINUTES)
                .map(|_| Mutex::new(MinuteSlot::default()))
                .collect(),
        }
    }

    fn record(&self, rec: &UsageRecord) {
        let minute = rec.ts / 60_000;
        let mut slot = self.slots[(minute as usize) % MINUTES].lock().unwrap();
        if slot.minute != minute {
            slot.minute = minute;
            slot.cells.clear();
        }
        if slot.cells.len() >= MAX_KEYS_PER_MINUTE {
            return; // cardinality cap; totals for hot keys still accrue
        }
        let cell = slot
            .cells
            .entry(AggKey {
                provider: rec.provider.clone(),
                model: rec.model.clone(),
                vkey: rec.vkey.clone(),
            })
            .or_default();
        cell.requests += 1;
        if rec.status >= 400 {
            cell.errors += 1;
        }
        cell.input_tokens += rec.input_tokens;
        cell.output_tokens += rec.output_tokens;
        cell.cost_micro_usd += rec.cost_micro_usd;
        cell.latency_ms_sum += rec.latency_ms;
    }

    /// Aggregate the trailing window, grouped by the requested dimensions
    /// (`provider`, `model`, `key`), returning per-minute series plus
    /// group totals.
    pub fn query(&self, now_ms: u64, window_secs: u64, by: &[&str], vkey: Option<&str>) -> Value {
        let window_minutes = (window_secs / 60).clamp(1, MINUTES as u64);
        let end_minute = now_ms / 60_000;
        let start_minute = end_minute.saturating_sub(window_minutes - 1);

        #[derive(Default)]
        struct Group {
            totals: AggCell,
            series: BTreeMap<u64, AggCell>,
        }
        let mut groups: BTreeMap<String, Group> = BTreeMap::new();

        for minute in start_minute..=end_minute {
            let slot = self.slots[(minute as usize) % MINUTES].lock().unwrap();
            if slot.minute != minute {
                continue;
            }
            for (key, cell) in &slot.cells {
                if let Some(want) = vkey
                    && key.vkey.as_deref() != Some(want)
                {
                    continue;
                }
                let mut parts = Vec::new();
                for dim in by {
                    match *dim {
                        "provider" => parts.push(key.provider.clone()),
                        "model" => parts.push(format!("{}/{}", key.provider, key.model)),
                        "key" => parts.push(key.vkey.clone().unwrap_or_else(|| "-".into())),
                        _ => {}
                    }
                }
                let group = groups.entry(parts.join("·")).or_default();
                for target in [&mut group.totals, group.series.entry(minute).or_default()] {
                    target.requests += cell.requests;
                    target.errors += cell.errors;
                    target.input_tokens += cell.input_tokens;
                    target.output_tokens += cell.output_tokens;
                    target.cost_micro_usd += cell.cost_micro_usd;
                    target.latency_ms_sum += cell.latency_ms_sum;
                }
            }
        }

        let cell_json = |c: &AggCell| {
            serde_json::json!({
                "requests": c.requests,
                "errors": c.errors,
                "input_tokens": c.input_tokens,
                "output_tokens": c.output_tokens,
                "cost_usd": c.cost_micro_usd as f64 / 1e6,
                "avg_latency_ms": if c.requests > 0 { c.latency_ms_sum / c.requests } else { 0 },
            })
        };
        let groups_json: Vec<Value> = groups
            .iter()
            .map(|(name, g)| {
                serde_json::json!({
                    "group": if name.is_empty() { "all" } else { name.as_str() },
                    "totals": cell_json(&g.totals),
                    "series": g.series.iter().map(|(minute, c)| {
                        let mut v = cell_json(c);
                        v["minute_ts"] = Value::from(minute * 60_000);
                        v
                    }).collect::<Vec<_>>(),
                })
            })
            .collect();
        serde_json::json!({
            "window_secs": window_secs,
            "groups": groups_json,
        })
    }
}

// ---------------------------------------------------------------------------
// The pipeline
// ---------------------------------------------------------------------------

/// Recent-request ring for the console's Requests page (metadata only).
const RECENT_CAP: usize = 1000;

pub struct UsagePipeline {
    tx: Mutex<Option<mpsc::SyncSender<UsageRecord>>>,
    pub agg: Aggregator,
    recent: Mutex<VecDeque<UsageRecord>>,
    dropped: AtomicU64,
    per_key_metrics: bool,
    key_label_cap: usize,
}

impl UsagePipeline {
    /// Start the pipeline. With a data dir, a flusher thread writes
    /// batches to disk and prunes retention; without one (pure env/file
    /// setups), aggregation is in-memory only.
    pub fn start(data_dir: Option<PathBuf>, cfg: &UsageConfig, node_id: &str) -> Arc<Self> {
        let tx = data_dir.map(|dir| {
            let (tx, rx) = mpsc::sync_channel::<UsageRecord>(8192);
            let flush_interval = cfg.flush_interval;
            let retention_days = cfg.retention_days;
            let node = node_id.to_owned();
            std::thread::Builder::new()
                .name("caret-usage-flush".into())
                .spawn(move || flusher(dir, rx, flush_interval, retention_days, node))
                .expect("spawn usage flusher");
            tx
        });
        Arc::new(Self {
            tx: Mutex::new(tx),
            agg: Aggregator::new(),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
            dropped: AtomicU64::new(0),
            per_key_metrics: cfg.per_key_metrics,
            key_label_cap: 100,
        })
    }

    pub fn record(&self, rec: UsageRecord) {
        self.agg.record(&rec);
        {
            let mut recent = self.recent.lock().unwrap();
            if recent.len() == RECENT_CAP {
                recent.pop_front();
            }
            recent.push_back(rec.clone());
        }
        metrics::counter!("caret_tokens_total", "kind" => "input").increment(rec.input_tokens);
        metrics::counter!("caret_tokens_total", "kind" => "output").increment(rec.output_tokens);
        if self.per_key_metrics
            && let Some(vk) = &rec.vkey
        {
            // Bounded cardinality: only while the recent ring holds fewer
            // distinct keys than the cap.
            let distinct = {
                let recent = self.recent.lock().unwrap();
                recent
                    .iter()
                    .filter_map(|r| r.vkey.as_deref())
                    .collect::<std::collections::BTreeSet<_>>()
                    .len()
            };
            if distinct <= self.key_label_cap {
                metrics::counter!("caret_tokens_total", "kind" => "total", "vkey" => vk.clone())
                    .increment(rec.input_tokens + rec.output_tokens);
            }
        }
        let tx = self.tx.lock().unwrap();
        if let Some(tx) = tx.as_ref()
            && tx.try_send(rec).is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("caret_usage_dropped_total").increment(1);
        }
    }

    pub fn recent(
        &self,
        limit: usize,
        vkey: Option<&str>,
        status_min: Option<u16>,
        provider: Option<&str>,
    ) -> Vec<UsageRecord> {
        let recent = self.recent.lock().unwrap();
        recent
            .iter()
            .rev()
            .filter(|r| vkey.is_none_or(|k| r.vkey.as_deref() == Some(k)))
            .filter(|r| status_min.is_none_or(|s| r.status >= s))
            .filter(|r| provider.is_none_or(|p| r.provider == p))
            .take(limit.min(RECENT_CAP))
            .cloned()
            .collect()
    }

    /// Sum of spend per virtual key for its current budget period, read
    /// back from the on-disk partitions at boot so restarts don't grant a
    /// fresh budget.
    pub fn seed_budgets(data_dir: &std::path::Path, table: &router_core::vkey::VkTable) {
        let now = vkey::unix_now_ms();
        for rt in table.iter() {
            if rt.def.budget.is_none() {
                continue;
            }
            let ordinal = rt.period_ordinal(now);
            let spent =
                scan_period_spend(data_dir, &rt.def.id, |ts| rt.period_ordinal(ts) == ordinal);
            if spent > 0 {
                rt.spend.add(spent, ordinal);
            }
        }
    }
}

/// Complete-request hook: carries everything needed to attribute a
/// response once its body has fully streamed. Fires exactly once — on
/// completion or on drop (client disconnect).
pub struct UsageHook {
    pub pipeline: Arc<UsagePipeline>,
    pub vkey: Option<Arc<VkRuntime>>,
    pub pricing: Pricing,
    pub events: Option<tokio::sync::broadcast::Sender<Value>>,
    pub request_id: String,
    pub endpoint: &'static str,
    pub requested: String,
    pub provider: String,
    pub model: String,
    pub stream: bool,
    pub attempts: u32,
    pub started: std::time::Instant,
    pub overhead_us: u64,
    pub tag: Option<String>,
}

impl UsageHook {
    pub fn complete(self, status: u16, usage: TokenUsage) {
        let now_unix = vkey::unix_now_ms();
        let cost = self
            .pricing
            .cost_micro_usd(&self.provider, &self.model, usage);
        if let Some(vk) = &self.vkey {
            let ordinal = vk.period_ordinal(now_unix);
            vk.debit_usage(
                usage.billable(),
                cost,
                router_core::clock::now_ms(),
                ordinal,
            );
        }
        let rec = UsageRecord {
            ts: now_unix,
            request_id: self.request_id,
            endpoint: self.endpoint.to_owned(),
            requested: self.requested,
            provider: self.provider,
            model: self.model,
            vkey: self.vkey.as_ref().map(|v| v.def.id.clone()),
            status,
            stream: self.stream,
            input_tokens: usage.input,
            output_tokens: usage.output,
            cached_tokens: usage.cached,
            cost_micro_usd: cost,
            latency_ms: self.started.elapsed().as_millis() as u64,
            overhead_us: self.overhead_us,
            attempts: self.attempts,
            tag: self.tag,
        };
        if let Some(events) = &self.events {
            let _ = events.send(serde_json::json!({
                "type": "request",
                "provider": rec.provider,
                "model": rec.model,
                "status": rec.status,
                "vkey": rec.vkey,
                "latency_ms": rec.latency_ms,
                "tokens": rec.input_tokens + rec.output_tokens,
                "cost_usd": rec.cost_micro_usd as f64 / 1e6,
                "ts": rec.ts,
            }));
        }
        self.pipeline.record(rec);
    }
}

/// Attach usage accounting to the response body. Completion is recorded
/// when the body ends or is dropped after a client disconnect. Only a
/// bounded tail is retained because providers place usage in the final
/// JSON object or final stream event.
pub fn meter_response(response: Response, hook: UsageHook, dialect: Dialect) -> Response {
    let (parts, inner) = response.into_parts();
    let body = MeteredBody {
        inner,
        hook: Some(hook),
        dialect,
        status: parts.status.as_u16(),
        // Grown on demand: most responses never reach the cap, and a
        // per-request pre-allocation is pure churn on the hot path.
        tail: Vec::new(),
    };
    Response::from_parts(parts, Body::new(body))
}

const USAGE_TAIL_CAP: usize = 256 * 1024;

struct MeteredBody {
    inner: Body,
    hook: Option<UsageHook>,
    dialect: Dialect,
    status: u16,
    tail: Vec<u8>,
}

impl MeteredBody {
    fn observe(&mut self, data: &[u8]) {
        if data.len() >= USAGE_TAIL_CAP {
            self.tail.clear();
            self.tail
                .extend_from_slice(&data[data.len() - USAGE_TAIL_CAP..]);
            return;
        }
        let overflow = self
            .tail
            .len()
            .saturating_add(data.len())
            .saturating_sub(USAGE_TAIL_CAP);
        if overflow > 0 {
            self.tail.drain(..overflow);
        }
        self.tail.extend_from_slice(data);
    }

    fn complete(&mut self) {
        let Some(hook) = self.hook.take() else {
            return;
        };
        let usage = if hook.stream {
            let mut scanner = StreamUsageScanner::new(self.dialect);
            if let Ok(text) = std::str::from_utf8(&self.tail) {
                for line in text.lines() {
                    if let Some(data) = line.strip_prefix("data:") {
                        scanner.on_event_data(data.trim_start());
                    }
                }
            }
            scanner.finish()
        } else {
            extract_sync(self.dialect, &self.tail)
        };
        hook.complete(self.status, usage);
    }
}

impl http_body::Body for MeteredBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        match std::pin::Pin::new(&mut self.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => {
                if let Some(data) = frame.data_ref() {
                    self.observe(data);
                }
                std::task::Poll::Ready(Some(Ok(frame)))
            }
            std::task::Poll::Ready(Some(Err(err))) => {
                self.complete();
                std::task::Poll::Ready(Some(Err(err)))
            }
            std::task::Poll::Ready(None) => {
                self.complete();
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.inner.size_hint()
    }
}

impl Drop for MeteredBody {
    fn drop(&mut self) {
        self.complete();
    }
}

// ---------------------------------------------------------------------------
// Disk: flusher thread, partitions, retention, boot-time scans
// ---------------------------------------------------------------------------

fn day_partition(ts_ms: u64) -> String {
    let days = ts_ms / 86_400_000;
    let (y, m, d) = civil(days as i64);
    format!("dt={y:04}-{m:02}-{d:02}")
}

fn civil(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn flusher(
    dir: PathBuf,
    rx: mpsc::Receiver<UsageRecord>,
    interval: Duration,
    retention_days: u32,
    node: String,
) {
    let mut seq: u64 = 0;
    let mut last_prune = std::time::Instant::now() - Duration::from_secs(3600);
    loop {
        // Block for the first record, then drain whatever accumulated
        // during the flush interval.
        let first = match rx.recv_timeout(interval.max(Duration::from_secs(1))) {
            Ok(rec) => Some(rec),
            Err(mpsc::RecvTimeoutError::Timeout) => None,
            Err(mpsc::RecvTimeoutError::Disconnected) => return,
        };
        if let Some(first) = first {
            std::thread::sleep(interval.min(Duration::from_secs(2)));
            let mut batch = vec![first];
            while let Ok(rec) = rx.try_recv() {
                batch.push(rec);
                if batch.len() >= 10_000 {
                    break;
                }
            }
            if let Err(err) = write_batch(&dir, &node, seq, &batch) {
                tracing::warn!(%err, "usage flush failed; records dropped");
            }
            seq += 1;
        }
        if last_prune.elapsed() > Duration::from_secs(3600) {
            last_prune = std::time::Instant::now();
            prune(&dir, retention_days);
        }
    }
}

fn write_batch(
    dir: &std::path::Path,
    node: &str,
    seq: u64,
    batch: &[UsageRecord],
) -> std::io::Result<()> {
    // Partition by record date (batches can straddle midnight).
    let mut by_day: BTreeMap<String, Vec<&UsageRecord>> = BTreeMap::new();
    for rec in batch {
        by_day.entry(day_partition(rec.ts)).or_default().push(rec);
    }
    for (day, records) in by_day {
        let day_dir = dir.join("usage").join(&day);
        std::fs::create_dir_all(&day_dir)?;
        let path = day_dir.join(format!("{node}-{seq:08}.jsonl.zst"));
        let file = std::fs::File::create(&path)?;
        let mut encoder = zstd::Encoder::new(file, 3)?;
        for rec in records {
            serde_json::to_writer(&mut encoder, rec)?;
            encoder.write_all(b"\n")?;
        }
        encoder.finish()?.sync_all()?;
    }
    Ok(())
}

fn prune(dir: &std::path::Path, retention_days: u32) {
    let usage_dir = dir.join("usage");
    let Ok(entries) = std::fs::read_dir(&usage_dir) else {
        return;
    };
    let cutoff =
        day_partition(vkey::unix_now_ms().saturating_sub(retention_days as u64 * 86_400_000));
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        // Lexicographic compare works for `dt=YYYY-MM-DD`.
        if name.starts_with("dt=") && name < cutoff.as_str() {
            if let Err(err) = std::fs::remove_dir_all(entry.path()) {
                tracing::warn!(%err, partition = name, "usage retention prune failed");
            } else {
                tracing::info!(partition = name, "pruned usage partition past retention");
            }
        }
    }
}

/// Scan on-disk partitions for one key's spend inside its current budget
/// period. Runs once per key at boot; partitions outside the period are
/// skipped by directory name.
fn scan_period_spend(dir: &std::path::Path, key_id: &str, in_period: impl Fn(u64) -> bool) -> u64 {
    let usage_dir = dir.join("usage");
    let Ok(days) = std::fs::read_dir(&usage_dir) else {
        return 0;
    };
    let mut total = 0u64;
    for day in days.flatten() {
        let Ok(files) = std::fs::read_dir(day.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("zst") {
                continue;
            }
            let Ok(f) = std::fs::File::open(&path) else {
                continue;
            };
            let Ok(reader) = zstd::Decoder::new(f) else {
                continue;
            };
            let reader = std::io::BufReader::new(reader);
            for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
                let Ok(rec) = serde_json::from_str::<UsageRecord>(&line) else {
                    continue;
                };
                if rec.vkey.as_deref() == Some(key_id) && in_period(rec.ts) {
                    total += rec.cost_micro_usd;
                }
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_sync_and_responses_shapes() {
        let body = br#"{"usage":{"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":4}}}"#;
        assert_eq!(
            extract_sync(Dialect::OpenAi, body),
            TokenUsage {
                input: 10,
                output: 5,
                cached: 4
            }
        );
        let responses = br#"{"usage":{"input_tokens":7,"output_tokens":3}}"#;
        assert_eq!(
            extract_sync(Dialect::OpenAi, responses),
            TokenUsage {
                input: 7,
                output: 3,
                cached: 0
            }
        );
        assert_eq!(extract_sync(Dialect::OpenAi, b"{}"), TokenUsage::default());
    }

    #[test]
    fn anthropic_gemini_bedrock_shapes() {
        let ant = br#"{"usage":{"input_tokens":20,"output_tokens":9,"cache_read_input_tokens":6}}"#;
        assert_eq!(
            extract_sync(Dialect::Anthropic, ant),
            TokenUsage {
                input: 20,
                output: 9,
                cached: 6
            }
        );
        let gem = br#"{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":4}}"#;
        assert_eq!(
            extract_sync(Dialect::Gemini, gem),
            TokenUsage {
                input: 11,
                output: 4,
                cached: 0
            }
        );
        let bed = br#"{"usage":{"inputTokens":8,"outputTokens":2}}"#;
        assert_eq!(
            extract_sync(Dialect::Bedrock, bed),
            TokenUsage {
                input: 8,
                output: 2,
                cached: 0
            }
        );
    }

    #[test]
    fn stream_scanner_accumulates_across_dialect_events() {
        // Anthropic: input arrives in message_start, output grows in
        // message_delta; the final (largest) value wins.
        let mut scanner = StreamUsageScanner::new(Dialect::Anthropic);
        scanner.on_event_data(
            r#"{"type":"message_start","message":{"usage":{"input_tokens":25,"output_tokens":1}}}"#,
        );
        scanner.on_event_data(r#"{"type":"content_block_delta","delta":{"text":"hi"}}"#);
        scanner.on_event_data(r#"{"type":"message_delta","usage":{"output_tokens":42}}"#);
        assert_eq!(
            scanner.finish(),
            TokenUsage {
                input: 25,
                output: 42,
                cached: 0
            }
        );

        // OpenAI: the usage chunk near [DONE], including the Responses
        // nested shape.
        let mut scanner = StreamUsageScanner::new(Dialect::OpenAi);
        scanner.on_event_data(r#"{"choices":[{"delta":{"content":"x"}}]}"#);
        scanner
            .on_event_data(r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":13}}"#);
        assert_eq!(
            scanner.finish(),
            TokenUsage {
                input: 9,
                output: 13,
                cached: 0
            }
        );

        let mut scanner = StreamUsageScanner::new(Dialect::OpenAi);
        scanner.on_event_data(r#"{"type":"response.completed","response":{"usage":{"input_tokens":4,"output_tokens":6}}}"#);
        assert_eq!(
            scanner.finish(),
            TokenUsage {
                input: 4,
                output: 6,
                cached: 0
            }
        );
    }

    #[test]
    fn billable_excludes_cached() {
        let usage = TokenUsage {
            input: 100,
            output: 20,
            cached: 60,
        };
        assert_eq!(usage.billable(), 60);
        assert_eq!(usage.total(), 120);
    }

    #[test]
    fn pricing_longest_match_and_overrides() {
        let pricing = Pricing::default();
        let mini = pricing
            .price_for("openai", "gpt-4o-mini-2024-07-18")
            .unwrap();
        assert_eq!(mini.input_per_mtok, 0.15);
        let full = pricing.price_for("openai", "gpt-4o-2024-08-06").unwrap();
        assert_eq!(full.input_per_mtok, 2.50);
        assert!(pricing.price_for("openai", "unknown-model").is_none());

        // 1M input at $0.15/M + 1M output at $0.60/M = $0.75 = 750k µUSD.
        let usage = TokenUsage {
            input: 1_000_000,
            output: 1_000_000,
            cached: 0,
        };
        assert_eq!(
            pricing.cost_micro_usd("openai", "gpt-4o-mini", usage),
            750_000
        );
    }

    #[test]
    fn aggregator_windows_and_grouping() {
        let agg = Aggregator::new();
        let rec = |ts: u64, provider: &str, vkey: Option<&str>, cost: u64| UsageRecord {
            ts,
            request_id: "r".into(),
            endpoint: "chat".into(),
            requested: "m".into(),
            provider: provider.into(),
            model: "m1".into(),
            vkey: vkey.map(Into::into),
            status: 200,
            stream: false,
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
            cost_micro_usd: cost,
            latency_ms: 100,
            overhead_us: 8,
            attempts: 1,
            tag: None,
        };
        let now: u64 = 200 * 60_000;
        agg.record(&rec(now, "openai", Some("k1"), 500));
        agg.record(&rec(now - 60_000, "openai", Some("k1"), 300));
        agg.record(&rec(now - 60_000, "anthropic", Some("k2"), 200));
        agg.record(&rec(now - 7_200_000, "openai", Some("k1"), 999)); // outside 1h window

        let result = agg.query(now, 3600, &["provider"], None);
        let groups = result["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 2);
        let openai = groups.iter().find(|g| g["group"] == "openai").unwrap();
        assert_eq!(openai["totals"]["requests"], 2);
        assert_eq!(openai["totals"]["cost_usd"], 0.0008);

        // Filtered by key.
        let filtered = agg.query(now, 3600, &["key"], Some("k2"));
        let groups = filtered["groups"].as_array().unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0]["group"], "k2");
    }

    #[test]
    fn partitions_write_scan_and_prune() {
        let dir = tempfile::tempdir().unwrap();
        let now = vkey::unix_now_ms();
        let mk = |ts: u64, key: &str, cost: u64| UsageRecord {
            ts,
            request_id: "r".into(),
            endpoint: "chat".into(),
            requested: "m".into(),
            provider: "openai".into(),
            model: "gpt-4o-mini".into(),
            vkey: Some(key.into()),
            status: 200,
            stream: false,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
            cost_micro_usd: cost,
            latency_ms: 1,
            overhead_us: 1,
            attempts: 1,
            tag: None,
        };
        let old = now.saturating_sub(90 * 86_400_000);
        write_batch(
            dir.path(),
            "node",
            0,
            &[
                mk(now, "k1", 100),
                mk(now, "k1", 50),
                mk(now, "k2", 7),
                mk(old, "k1", 999),
            ],
        )
        .unwrap();

        // Scan: only k1 records inside the "period" (here: today+).
        let spent = scan_period_spend(dir.path(), "k1", |ts| ts >= now - 86_400_000);
        assert_eq!(spent, 150);

        // Prune drops the 90-day-old partition, keeps today's.
        prune(dir.path(), 30);
        let partitions: Vec<String> = std::fs::read_dir(dir.path().join("usage"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions[0], day_partition(now));
    }
}
