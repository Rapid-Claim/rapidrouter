//! The usage pipeline: token/cost extraction from upstream responses, the
//! hot-path record ring, in-memory aggregation windows, and durable
//! date-partitioned JSONL on disk.
//!
//! The hot path does one bounded channel send; a dedicated flusher thread
//! batches records into `usage/dt=YYYY-MM-DD/*.jsonl.zst` and prunes
//! partitions past retention. Budgets are enforced from the in-memory
//! spend counters, seeded from disk at boot — cutoff lag is bounded by the
//! flush interval, the documented cost of "no database."

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

use axum::body::Body;
use axum::response::Response;
use bytes::Bytes;
use http_body::Frame;
use router_core::config::{BodyCapture, Config, Price, UsageConfig};
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
/// Public list prices in USD per million tokens, matched by the longest
/// substring of the model name.
///
/// A fallback, not the source of truth. Prices change and models ship
/// weekly, so the catalog fetched at runtime
/// ([`Pricing::refresh_catalog`]) takes precedence over this, and an
/// explicit `[pricing]` entry takes precedence over both. What this
/// table buys is a gateway that reports sane costs on first run, before
/// it has talked to anything.
///
/// Ordering does not matter — the longest matching needle wins, so
/// `gpt-4o-mini` beats `gpt-4o`.
const BUILTIN_PRICES: &[(&str, f64, f64)] = &[
    // OpenAI
    ("gpt-5-nano", 0.05, 0.40),
    ("gpt-5-mini", 0.25, 2.00),
    ("gpt-5-codex", 1.25, 10.00),
    ("gpt-5-chat", 1.25, 10.00),
    ("gpt-5", 1.25, 10.00),
    ("gpt-4.1-nano", 0.10, 0.40),
    ("gpt-4.1-mini", 0.40, 1.60),
    ("gpt-4.1", 2.00, 8.00),
    ("gpt-4o-mini", 0.15, 0.60),
    ("gpt-4o", 2.50, 10.00),
    ("gpt-4-turbo", 10.00, 30.00),
    ("gpt-3.5-turbo", 0.50, 1.50),
    ("o4-mini", 1.10, 4.40),
    ("o3-mini", 1.10, 4.40),
    ("o3-pro", 20.00, 80.00),
    ("o3", 2.00, 8.00),
    ("o1-mini", 1.10, 4.40),
    ("o1-pro", 150.00, 600.00),
    ("o1", 15.00, 60.00),
    ("text-embedding-3-large", 0.13, 0.0),
    ("text-embedding-3-small", 0.02, 0.0),
    // Anthropic
    ("claude-haiku-4-5", 1.00, 5.00),
    ("claude-3-5-haiku", 0.80, 4.00),
    ("claude-3-haiku", 0.25, 1.25),
    ("claude-haiku", 1.00, 5.00),
    ("claude-sonnet-4-5", 3.00, 15.00),
    ("claude-3-7-sonnet", 3.00, 15.00),
    ("claude-3-5-sonnet", 3.00, 15.00),
    ("claude-sonnet", 3.00, 15.00),
    ("claude-opus-4-1", 15.00, 75.00),
    ("claude-opus-4", 15.00, 75.00),
    ("claude-3-opus", 15.00, 75.00),
    ("claude-opus", 15.00, 75.00),
    // Google
    ("gemini-2.5-flash-lite", 0.10, 0.40),
    ("gemini-2.5-flash", 0.30, 2.50),
    ("gemini-2.5-pro", 1.25, 10.00),
    ("gemini-2.0-flash-lite", 0.075, 0.30),
    ("gemini-2.0-flash", 0.10, 0.40),
    ("gemini-1.5-flash", 0.075, 0.30),
    ("gemini-1.5-pro", 1.25, 5.00),
    ("gemini-flash-lite", 0.10, 0.40),
    ("gemini-flash", 0.30, 2.50),
    ("gemini-pro", 1.25, 10.00),
    // Meta / Mistral / DeepSeek / xAI, as commonly served
    ("llama-3.3-70b", 0.59, 0.79),
    ("llama-3.1-405b", 3.50, 3.50),
    ("llama-3.1-70b", 0.59, 0.79),
    ("llama-3.1-8b", 0.05, 0.08),
    ("mistral-large", 2.00, 6.00),
    ("mistral-small", 0.20, 0.60),
    ("mixtral-8x7b", 0.24, 0.24),
    ("deepseek-reasoner", 0.55, 2.19),
    ("deepseek-chat", 0.27, 1.10),
    ("grok-4", 3.00, 15.00),
    ("grok-3-mini", 0.30, 0.50),
    ("grok-3", 3.00, 15.00),
];

/// Where model prices are fetched from when no other source is set.
///
/// The community-maintained aggregate, because it is the only feed that
/// covers every provider in one document and tracks new models within
/// days of release. Providers publish prices on marketing pages, not as
/// machine-readable feeds, so scraping each one would be more fragile
/// than reading one aggregate that already does it.
pub const DEFAULT_PRICE_CATALOG_URL: &str =
    "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";

#[derive(Debug, Default, Clone)]
pub struct Pricing {
    overrides: BTreeMap<String, Price>,
    /// Prices fetched from a public catalog at runtime, keyed by the
    /// model id the catalog uses.
    catalog: Arc<BTreeMap<String, Price>>,
}

impl Pricing {
    pub fn from_config(config: &Config) -> Self {
        Self {
            overrides: config.pricing.clone(),
            catalog: Arc::new(BTreeMap::new()),
        }
    }

    /// Same pricing, with a freshly fetched catalog swapped in.
    pub fn with_catalog(&self, catalog: Arc<BTreeMap<String, Price>>) -> Self {
        Self {
            overrides: self.overrides.clone(),
            catalog,
        }
    }

    pub fn catalog_len(&self) -> usize {
        self.catalog.len()
    }

    /// Three sources, most specific first: an explicit `[pricing]` entry,
    /// the catalog fetched at runtime, then the built-in table.
    ///
    /// An operator who has written a price down means it — a negotiated
    /// rate is not something a public catalog should silently override.
    pub fn price_for(&self, provider: &str, model: &str) -> Option<Price> {
        if let Some(p) = self.overrides.get(&format!("{provider}/{model}")) {
            return Some(*p);
        }
        if let Some(p) = self.overrides.get(model) {
            return Some(*p);
        }
        if let Some(p) = self
            .catalog
            .get(model)
            .or_else(|| self.catalog.get(&format!("{provider}/{model}")))
        {
            return Some(*p);
        }
        // A catalog id can carry a provider prefix or a dated suffix, so
        // fall back to the longest catalog key the model name contains.
        if let Some((_, price)) = self
            .catalog
            .iter()
            .filter(|(id, _)| {
                let bare = id.rsplit('/').next().unwrap_or(id);
                !bare.is_empty() && model.contains(bare)
            })
            .max_by_key(|(id, _)| id.rsplit('/').next().unwrap_or(id).len())
        {
            return Some(*price);
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

    /// Parse a public price feed into a catalog.
    ///
    /// The shape is the community `model_prices_and_context_window.json`
    /// one — a map of model id to a record with per-token costs — because
    /// it is the only machine-readable feed that covers every provider at
    /// once and is updated as models ship. Entries without both costs are
    /// skipped rather than defaulted: a model priced at zero because its
    /// feed entry was incomplete would under-report spend, which is worse
    /// than reporting nothing.
    pub fn parse_catalog(body: &[u8]) -> Result<BTreeMap<String, Price>, String> {
        let raw: BTreeMap<String, serde_json::Value> =
            serde_json::from_slice(body).map_err(|e| e.to_string())?;
        let mut catalog = BTreeMap::new();
        for (id, entry) in raw {
            let input = entry.get("input_cost_per_token").and_then(|v| v.as_f64());
            let output = entry.get("output_cost_per_token").and_then(|v| v.as_f64());
            let (Some(input), Some(output)) = (input, output) else {
                continue;
            };
            catalog.insert(
                id,
                Price {
                    // Feeds quote per token; everything here is per Mtok.
                    input_per_mtok: input * 1_000_000.0,
                    output_per_mtok: output * 1_000_000.0,
                },
            );
        }
        Ok(catalog)
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
    /// Where the flusher writes, so history can be read back. `None` when
    /// there is no data dir and aggregation is in-memory only.
    data_dir: Option<PathBuf>,
    tx: Mutex<Option<mpsc::SyncSender<UsageRecord>>>,
    pub agg: Aggregator,
    recent: Mutex<VecDeque<UsageRecord>>,
    dropped: AtomicU64,
    per_key_metrics: bool,
    key_label_cap: usize,
    /// Other nodes' rollup rows, refreshed by the shipper. Empty on a
    /// single-node gateway, which is why history works without a store.
    fleet: Mutex<Vec<RollupRow>>,
    body_tx: Mutex<Option<mpsc::SyncSender<RequestBodies>>>,
    capture: BodyCapture,
    body_limit: usize,
}

impl UsagePipeline {
    /// Start the pipeline. With a data dir, a flusher thread writes
    /// batches to disk and prunes retention; without one (pure env/file
    /// setups), aggregation is in-memory only.
    pub fn start(data_dir: Option<PathBuf>, cfg: &UsageConfig, node_id: &str) -> Arc<Self> {
        let history_dir = data_dir.clone();
        let mut body_tx = None;
        let tx = data_dir.map(|dir| {
            let (tx, rx) = mpsc::sync_channel::<UsageRecord>(8192);
            // A shallower queue than the metadata one: bodies are large,
            // and a backlog of them is memory. Dropping a body under
            // pressure costs a debugging view; dropping a record would
            // cost money accounting.
            let (btx, brx) = mpsc::sync_channel::<RequestBodies>(1024);
            body_tx = Some(btx);
            let flush_interval = cfg.flush_interval;
            let retention_days = cfg.retention_days;
            let body_retention_days = cfg.body_retention_days;
            let node = node_id.to_owned();
            std::thread::Builder::new()
                .name("rapid-usage-flush".into())
                .spawn(move || {
                    flusher(
                        dir,
                        rx,
                        brx,
                        flush_interval,
                        retention_days,
                        body_retention_days,
                        node,
                    )
                })
                .expect("spawn usage flusher");
            tx
        });
        Arc::new(Self {
            data_dir: history_dir,
            tx: Mutex::new(tx),
            body_tx: Mutex::new(body_tx),
            capture: cfg.capture_bodies,
            body_limit: cfg.body_limit_bytes,
            agg: Aggregator::new(),
            recent: Mutex::new(VecDeque::with_capacity(RECENT_CAP)),
            fleet: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            per_key_metrics: cfg.per_key_metrics,
            key_label_cap: 100,
        })
    }

    /// Bytes this pipeline will keep for a response with this status —
    /// zero when capture is off, so callers can skip the work entirely.
    pub fn capture_limit_for(&self, status: u16) -> usize {
        if self.capture.wants(status) {
            self.body_limit
        } else {
            0
        }
    }

    /// Store one request's bodies, if capture is on for this status.
    ///
    /// Never blocks the caller: the queue is bounded and a full queue
    /// drops the body rather than stalling a response. The record itself
    /// always survives — accounting is not allowed to depend on whether
    /// there was room for a prompt.
    pub fn record_bodies(&self, request_id: &str, ts: u64, status: u16, input: &str, output: &str) {
        if !self.capture.wants(status) {
            return;
        }
        let Ok(guard) = self.body_tx.lock() else {
            return;
        };
        let Some(tx) = guard.as_ref() else {
            return;
        };
        let (input, cut_in) = cap_body(input, self.body_limit);
        let (output, cut_out) = cap_body(output, self.body_limit);
        let _ = tx.try_send(RequestBodies {
            request_id: request_id.to_owned(),
            ts,
            input,
            output,
            truncated: cut_in || cut_out,
        });
    }

    /// The stored bodies for one request, if they were captured and are
    /// still inside their retention window.
    pub fn bodies_for(&self, request_id: &str, ts: u64) -> Option<RequestBodies> {
        let dir = self
            .data_dir
            .as_ref()?
            .join("bodies")
            .join(day_partition(ts));
        let files = std::fs::read_dir(dir).ok()?;
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
            for line in
                std::io::BufRead::lines(std::io::BufReader::new(decoder)).map_while(Result::ok)
            {
                if !line.contains(request_id) {
                    continue;
                }
                if let Ok(bodies) = serde_json::from_str::<RequestBodies>(&line)
                    && bodies.request_id == request_id
                {
                    return Some(bodies);
                }
            }
        }
        None
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
        metrics::counter!("rapid_tokens_total", "kind" => "input").increment(rec.input_tokens);
        metrics::counter!("rapid_tokens_total", "kind" => "output").increment(rec.output_tokens);
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
                metrics::counter!("rapid_tokens_total", "kind" => "total", "vkey" => vk.clone())
                    .increment(rec.input_tokens + rec.output_tokens);
            }
        }
        let tx = self.tx.lock().unwrap();
        if let Some(tx) = tx.as_ref()
            && tx.try_send(rec).is_err()
        {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            metrics::counter!("rapid_usage_dropped_total").increment(1);
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
    /// The credential that served this request, when one did.
    pub seat: Option<crate::proxy::SeatUsed>,
    /// The request body as the caller sent it, kept for the log drawer.
    pub input_body: Option<String>,
}

impl UsageHook {
    pub fn complete(self, status: u16, usage: TokenUsage) {
        self.complete_with_body(status, usage, None);
    }

    /// Complete, and store the exchange when body capture is on.
    pub fn complete_with_body(self, status: u16, usage: TokenUsage, output: Option<&str>) {
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
        // Tokens are post-paid: the ceiling is only enforceable once the
        // response says what it actually cost.
        if let Some(seat) = &self.seat
            && let Some(key) = seat.provider.keys.iter().find(|k| k.name == seat.key)
        {
            key.debit_tokens(usage.billable(), router_core::clock::now_ms());
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
        // Bodies are stored against the same request id the record
        // carries, which is what the drawer looks them up by.
        self.pipeline.record_bodies(
            &rec.request_id,
            rec.ts,
            status,
            self.input_body.as_deref().unwrap_or_default(),
            output.unwrap_or_default(),
        );
        self.pipeline.record(rec);
    }
}

/// Attach usage accounting to the response body. Completion is recorded
/// when the body ends or is dropped after a client disconnect. Only a
/// bounded tail is retained because providers place usage in the final
/// JSON object or final stream event.
pub fn meter_response(response: Response, hook: UsageHook, dialect: Dialect) -> Response {
    let (parts, inner) = response.into_parts();
    // Zero when capture is off, which turns the whole path into a
    // comparison per chunk.
    let hook_capture_limit = hook.pipeline.capture_limit_for(parts.status.as_u16());
    let body = MeteredBody {
        inner,
        hook: Some(hook),
        dialect,
        status: parts.status.as_u16(),
        // Grown on demand: most responses never reach the cap, and a
        // per-request pre-allocation is pure churn on the hot path.
        tail: Vec::new(),
        capture_limit: hook_capture_limit,
        captured: Vec::new(),
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
    /// The head of the response, kept for the log drawer.
    ///
    /// The head rather than the tail the token scanner keeps: usage is
    /// reported in the last frames, but the part of an answer a person
    /// wants to read starts at the beginning.
    captured: Vec<u8>,
    capture_limit: usize,
}

impl MeteredBody {
    fn observe(&mut self, data: &[u8]) {
        if self.captured.len() < self.capture_limit {
            let room = self.capture_limit - self.captured.len();
            self.captured
                .extend_from_slice(&data[..room.min(data.len())]);
        }
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
        let captured = std::mem::take(&mut self.captured);
        let output = String::from_utf8_lossy(&captured).into_owned();
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
        hook.complete_with_body(self.status, usage, Some(&output));
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

/// One hour of traffic for one (provider, model, key) combination.
///
/// A fact row at hour granularity with every dimension present, rather
/// than one pre-grouped series per dimension. Keeping the dimensions
/// means any grouping *and* any filter the console offers can be
/// answered from the same rows — "tokens by model, for one provider and
/// one key" is a group-by over these, not a second rollup that has to be
/// designed in advance.
///
/// The row count is bounded by combinations actually served, not by
/// traffic: a million requests an hour across twenty models and fifty
/// keys is at most a thousand rows, and in practice far fewer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RollupRow {
    /// Start of the hour, unix milliseconds.
    pub hour_ms: u64,
    pub provider: String,
    pub model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vkey: Option<String>,
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micro_usd: u64,
    /// Summed, not averaged: averages do not compose across rows, and
    /// the console divides by `requests` at read time.
    pub latency_ms_sum: u64,
}

const HOUR_MS: u64 = 3_600_000;

/// Fold a batch of records into hour rows.
fn roll_up(batch: &[UsageRecord]) -> Vec<RollupRow> {
    let mut rows: BTreeMap<(u64, String, String, Option<String>), RollupRow> = BTreeMap::new();
    for rec in batch {
        let hour_ms = rec.ts - (rec.ts % HOUR_MS);
        let key = (
            hour_ms,
            rec.provider.clone(),
            rec.model.clone(),
            rec.vkey.clone(),
        );
        let row = rows.entry(key).or_insert_with(|| RollupRow {
            hour_ms,
            provider: rec.provider.clone(),
            model: rec.model.clone(),
            vkey: rec.vkey.clone(),
            requests: 0,
            failed: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cost_micro_usd: 0,
            latency_ms_sum: 0,
        });
        row.requests += 1;
        if rec.status >= 400 {
            row.failed += 1;
        }
        row.input_tokens += rec.input_tokens;
        row.output_tokens += rec.output_tokens;
        row.cached_tokens += rec.cached_tokens;
        row.cost_micro_usd += rec.cost_micro_usd;
        row.latency_ms_sum += rec.latency_ms;
    }
    rows.into_values().collect()
}

/// Append rollup rows for a batch, partitioned by day like the raw
/// records.
///
/// Appended as deltas and summed on read rather than rewritten in place:
/// a rewrite would have to read, merge and replace a file that another
/// flush may be writing, and a torn rewrite loses history. Deltas make a
/// crash cost at most the current batch.
fn write_rollups(
    dir: &std::path::Path,
    node: &str,
    seq: u64,
    rows: &[RollupRow],
) -> std::io::Result<()> {
    let mut by_day: BTreeMap<String, Vec<&RollupRow>> = BTreeMap::new();
    for row in rows {
        by_day
            .entry(day_partition(row.hour_ms))
            .or_default()
            .push(row);
    }
    for (day, rows) in by_day {
        let day_dir = dir.join("rollup").join(&day);
        std::fs::create_dir_all(&day_dir)?;
        let path = day_dir.join(format!("{node}-{seq:08}.jsonl.zst"));
        let file = std::fs::File::create(&path)?;
        let mut encoder = zstd::Encoder::new(file, 3)?;
        for row in rows {
            serde_json::to_writer(&mut encoder, row)?;
            encoder.write_all(
                b"
",
            )?;
        }
        encoder.finish()?.sync_all()?;
    }
    Ok(())
}

fn flusher(
    dir: PathBuf,
    rx: mpsc::Receiver<UsageRecord>,
    bodies_rx: mpsc::Receiver<RequestBodies>,
    interval: Duration,
    retention_days: u32,
    body_retention_days: u32,
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
            // Rollups are written from the same batch, so the aggregate
            // can never drift from the records it summarises.
            if let Err(err) = write_rollups(&dir, &node, seq, &roll_up(&batch)) {
                tracing::warn!(%err, "usage rollup failed; charts will fall back to a raw scan");
            }
            // Bodies ride the same beat but their own stream, so a log
            // listing never reads them.
            let mut bodies = Vec::new();
            while let Ok(body) = bodies_rx.try_recv() {
                bodies.push(body);
                if bodies.len() >= 10_000 {
                    break;
                }
            }
            if !bodies.is_empty()
                && let Err(err) = write_bodies(&dir, &node, seq, &bodies)
            {
                tracing::warn!(%err, "request bodies could not be written");
            }
            seq += 1;
        }
        if last_prune.elapsed() > Duration::from_secs(3600) {
            last_prune = std::time::Instant::now();
            prune(&dir, retention_days, body_retention_days);
        }
    }
}

/// Record-level constraints for a history read. Empty fields constrain
/// nothing.
#[derive(Debug, Default)]
pub struct HistoryFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub vkey: Option<String>,
}

impl HistoryFilter {
    /// The same constraints applied to a rollup row's dimensions.
    fn matches_dims(&self, provider: &str, model: &str, vkey: Option<&str>) -> bool {
        self.provider.as_deref().is_none_or(|p| provider == p)
            && self.model.as_deref().is_none_or(|m| model == m)
            && self.vkey.as_deref().is_none_or(|k| vkey == Some(k))
    }

    fn matches(&self, rec: &UsageRecord) -> bool {
        self.provider.as_deref().is_none_or(|p| rec.provider == p)
            && self.model.as_deref().is_none_or(|m| rec.model == m)
            && self
                .vkey
                .as_deref()
                .is_none_or(|k| rec.vkey.as_deref() == Some(k))
    }
}

/// One day's totals, optionally split by a dimension.
#[derive(Debug, Serialize, Default)]
pub struct DayBucket {
    pub day: String,
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
}

impl UsagePipeline {
    /// Daily totals over the last `days`, grouped by `by`
    /// (`provider` | `model` | `key` | `""` for the total), optionally
    /// filtered per record before bucketing.
    ///
    /// The filter runs against raw records, so it composes with any
    /// grouping — "tokens by model, for one provider and one key" is the
    /// same walk as the unfiltered read. Read from the flushed files
    /// rather than the in-memory aggregate, which only spans 24 hours;
    /// records still in the current batch firm up within a flush
    /// interval.
    /// A pipeline that only reads history, for tests and tooling.
    #[doc(hidden)]
    pub fn for_test(data_dir: Option<PathBuf>) -> Self {
        Self {
            data_dir,
            tx: Mutex::new(None),
            agg: Aggregator::new(),
            recent: Mutex::new(VecDeque::new()),
            fleet: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            per_key_metrics: false,
            key_label_cap: 0,
            body_tx: Mutex::new(None),
            capture: BodyCapture::Off,
            body_limit: 0,
        }
    }

    /// Recent requests, from memory and then from disk.
    ///
    /// The in-memory ring holds the last thousand records, which at a
    /// million requests a day is about ninety seconds — fine for a live
    /// tail and useless for "what happened this morning". Anything the
    /// ring cannot answer is read from the flushed partitions, newest
    /// file first, stopping as soon as `limit` matches are found.
    ///
    /// Newest-first with an early exit is what keeps this bounded: the
    /// flusher writes each batch to its own increasing sequence file and
    /// never reopens one, so file order is time order, and a filtered
    /// search over a busy day usually reads a handful of files rather
    /// than the day.
    pub fn recent_from_disk(
        &self,
        limit: usize,
        since_ms: u64,
        until_ms: u64,
        filter: &HistoryFilter,
        errors_only: bool,
    ) -> Vec<UsageRecord> {
        self.page_from_disk(limit, since_ms, until_ms, filter, errors_only, None)
            .0
    }

    /// One page of requests, newest first, plus the cursor for the next.
    ///
    /// Paged by (timestamp, request id) rather than an offset: an offset
    /// re-reads and re-skips everything before it, so page 50 costs fifty
    /// times page 1 — and with new records arriving at the head, offsets
    /// also shift under the reader and duplicate rows across pages. A
    /// cursor is a position in the data, so every page costs the same and
    /// nothing is shown twice or skipped.
    pub fn page_from_disk(
        &self,
        limit: usize,
        since_ms: u64,
        until_ms: u64,
        filter: &HistoryFilter,
        errors_only: bool,
        after: Option<(u64, String)>,
    ) -> (Vec<UsageRecord>, Option<(u64, String)>) {
        let page = self.collect_page(limit + 1, since_ms, until_ms, filter, errors_only, after);
        let has_more = page.len() > limit;
        let mut page = page;
        page.truncate(limit);
        let cursor = has_more
            .then(|| page.last().map(|r| (r.ts, r.request_id.clone())))
            .flatten();
        (page, cursor)
    }

    fn collect_page(
        &self,
        limit: usize,
        since_ms: u64,
        until_ms: u64,
        filter: &HistoryFilter,
        errors_only: bool,
        after: Option<(u64, String)>,
    ) -> Vec<UsageRecord> {
        let mut out: Vec<UsageRecord> = Vec::new();
        let keep = |rec: &UsageRecord| {
            if rec.ts < since_ms || rec.ts > until_ms {
                return false;
            }
            // Strictly after the cursor in (ts desc, id desc) order, so a
            // second of traffic that shares a timestamp is not re-shown.
            if let Some((cursor_ts, cursor_id)) = &after {
                let position = (rec.ts, rec.request_id.as_str());
                if position >= (*cursor_ts, cursor_id.as_str()) {
                    return false;
                }
            }
            filter.matches(rec) && (!errors_only || rec.status >= 400)
        };

        if let Ok(recent) = self.recent.lock() {
            for rec in recent.iter().rev() {
                if keep(rec) {
                    out.push(rec.clone());
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        let oldest_in_memory = out.last().map(|r| r.ts).unwrap_or(u64::MAX);

        let Some(root) = self.data_dir.as_ref().map(|d| d.join("usage")) else {
            return out;
        };
        let cutoff = day_partition(since_ms);
        let mut days = partitions_since(&root, &cutoff);
        days.reverse();
        for (_, day_path) in days {
            let Ok(entries) = std::fs::read_dir(&day_path) else {
                continue;
            };
            let mut files: Vec<_> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("zst"))
                .collect();
            // Sequence numbers are zero-padded, so a reverse sort is
            // newest-first.
            files.sort();
            files.reverse();
            for path in files {
                let Ok(handle) = std::fs::File::open(&path) else {
                    continue;
                };
                let Ok(decoder) = zstd::Decoder::new(handle) else {
                    continue;
                };
                let mut batch: Vec<UsageRecord> =
                    std::io::BufRead::lines(std::io::BufReader::new(decoder))
                        .map_while(Result::ok)
                        .filter_map(|line| serde_json::from_str::<UsageRecord>(&line).ok())
                        // A record already served from memory must not appear
                        // twice; the ring and the files overlap by design.
                        .filter(|rec| rec.ts < oldest_in_memory && keep(rec))
                        .collect();
                batch.sort_by(|a, b| b.ts.cmp(&a.ts));
                for rec in batch {
                    out.push(rec);
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    /// Replace the cached view of what other nodes have recorded.
    pub fn set_fleet_rollups(&self, rows: Vec<RollupRow>) {
        if let Ok(mut fleet) = self.fleet.lock() {
            *fleet = rows;
        }
    }

    /// Daily series for the console, read from hourly rollups.
    ///
    /// Rollups are two orders of magnitude smaller than the records they
    /// summarise — a week is thousands of rows rather than millions — so
    /// this is a bounded read whatever the traffic. Days with no rollup
    /// (written before rollups existed, or by an older node) fall back to
    /// scanning the raw records for that day, so history never has a
    /// hole; the fallback simply costs what it always did.
    pub fn history(
        &self,
        days: u32,
        by: &str,
        filter: &HistoryFilter,
    ) -> BTreeMap<String, Vec<DayBucket>> {
        let Some(root) = self.data_dir.clone() else {
            return BTreeMap::new();
        };
        let cutoff =
            day_partition(vkey::unix_now_ms().saturating_sub(days.max(1) as u64 * 86_400_000));
        let mut out: BTreeMap<String, BTreeMap<String, DayBucket>> = BTreeMap::new();

        let rollup_days = partitions_since(&root.join("rollup"), &cutoff);
        let mut covered: BTreeSet<String> = BTreeSet::new();
        for (partition, path) in rollup_days {
            let day = partition.trim_start_matches("dt=").to_owned();
            covered.insert(day.clone());
            for row in read_rollup_dir(&path) {
                if !filter.matches_dims(&row.provider, &row.model, row.vkey.as_deref()) {
                    continue;
                }
                let series = match by {
                    "provider" => row.provider.clone(),
                    "model" => row.model.clone(),
                    "key" => row.vkey.clone().unwrap_or_else(|| "(none)".into()),
                    _ => "total".to_owned(),
                };
                let bucket = out
                    .entry(series)
                    .or_default()
                    .entry(day.clone())
                    .or_insert_with(|| DayBucket {
                        day: day.clone(),
                        ..Default::default()
                    });
                bucket.requests += row.requests;
                bucket.failed += row.failed;
                bucket.input_tokens += row.input_tokens;
                bucket.output_tokens += row.output_tokens;
                bucket.cost_micro_usd += row.cost_micro_usd;
            }
        }

        // Other nodes' rollups for the same window. Their days are
        // additive, never duplicative: a row is written by exactly the
        // node that served the traffic.
        if let Ok(fleet) = self.fleet.lock() {
            for row in fleet.iter() {
                let day = day_partition(row.hour_ms);
                let day = day.trim_start_matches("dt=").to_owned();
                if day.as_str() < cutoff.trim_start_matches("dt=") {
                    continue;
                }
                if !filter.matches_dims(&row.provider, &row.model, row.vkey.as_deref()) {
                    continue;
                }
                let series = match by {
                    "provider" => row.provider.clone(),
                    "model" => row.model.clone(),
                    "key" => row.vkey.clone().unwrap_or_else(|| "(none)".into()),
                    _ => "total".to_owned(),
                };
                let bucket = out
                    .entry(series)
                    .or_default()
                    .entry(day.clone())
                    .or_insert_with(|| DayBucket {
                        day: day.clone(),
                        ..Default::default()
                    });
                bucket.requests += row.requests;
                bucket.failed += row.failed;
                bucket.input_tokens += row.input_tokens;
                bucket.output_tokens += row.output_tokens;
                bucket.cost_micro_usd += row.cost_micro_usd;
            }
        }

        // Any day inside the window without a rollup is scanned raw.
        let raw = self.history_from_records(days, by, filter, &covered);
        for (series, buckets) in raw {
            let entry = out.entry(series).or_default();
            for bucket in buckets {
                let day = bucket.day.clone();
                let slot = entry.entry(day.clone()).or_insert_with(|| DayBucket {
                    day,
                    ..Default::default()
                });
                slot.requests += bucket.requests;
                slot.failed += bucket.failed;
                slot.input_tokens += bucket.input_tokens;
                slot.output_tokens += bucket.output_tokens;
                slot.cost_micro_usd += bucket.cost_micro_usd;
            }
        }

        out.into_iter()
            .map(|(series, days)| (series, days.into_values().collect()))
            .collect()
    }

    /// The original full scan, kept for days that predate rollups.
    fn history_from_records(
        &self,
        days: u32,
        by: &str,
        filter: &HistoryFilter,
        skip_days: &BTreeSet<String>,
    ) -> BTreeMap<String, Vec<DayBucket>> {
        let mut out: BTreeMap<String, BTreeMap<String, DayBucket>> = BTreeMap::new();
        let Some(dir) = self.data_dir.as_ref().map(|d| d.join("usage")) else {
            return BTreeMap::new();
        };
        let cutoff =
            day_partition(vkey::unix_now_ms().saturating_sub(days.max(1) as u64 * 86_400_000));
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return BTreeMap::new();
        };
        let mut days_found: Vec<_> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                // Lexicographic compare works for `dt=YYYY-MM-DD`.
                (name.starts_with("dt=") && name.as_str() >= cutoff.as_str())
                    .then(|| (name, e.path()))
            })
            .collect();
        days_found.sort();

        for (partition, path) in days_found {
            let day = partition.trim_start_matches("dt=").to_owned();
            if skip_days.contains(&day) {
                continue;
            }
            let Ok(files) = std::fs::read_dir(&path) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(handle) = std::fs::File::open(file.path()) else {
                    continue;
                };
                let Ok(decoder) = zstd::Decoder::new(handle) else {
                    continue;
                };
                for line in
                    std::io::BufRead::lines(std::io::BufReader::new(decoder)).map_while(Result::ok)
                {
                    let Ok(rec) = serde_json::from_str::<UsageRecord>(&line) else {
                        continue;
                    };
                    if !filter.matches(&rec) {
                        continue;
                    }
                    let series = match by {
                        "provider" => rec.provider.clone(),
                        "model" => rec.model.clone(),
                        "key" => rec.vkey.clone().unwrap_or_else(|| "(none)".into()),
                        _ => "total".to_owned(),
                    };
                    let bucket = out
                        .entry(series)
                        .or_default()
                        .entry(day.clone())
                        .or_insert_with(|| DayBucket {
                            day: day.clone(),
                            ..Default::default()
                        });
                    bucket.requests += 1;
                    if rec.status >= 400 {
                        bucket.failed += 1;
                    }
                    bucket.input_tokens += rec.input_tokens;
                    bucket.output_tokens += rec.output_tokens;
                    bucket.cost_micro_usd += rec.cost_micro_usd;
                }
            }
        }
        out.into_iter()
            .map(|(series, days)| (series, days.into_values().collect()))
            .collect()
    }
}

/// A request's bodies, stored apart from its metadata.
///
/// Its own stream, keyed by request id: a log listing reads metadata for
/// hundreds of requests and needs none of this, and bodies are two
/// orders of magnitude larger. Mixing them would make every listing pay
/// for data only the drawer ever opens.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestBodies {
    pub request_id: String,
    pub ts: u64,
    pub input: String,
    pub output: String,
    /// Set when either body hit the size cap.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Truncate on a character boundary, marking what was cut.
fn cap_body(body: &str, limit: usize) -> (String, bool) {
    if body.len() <= limit {
        return (body.to_owned(), false);
    }
    let mut end = limit;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    (
        format!("{}\n… truncated at {limit} bytes", &body[..end]),
        true,
    )
}

/// Append bodies for a batch, partitioned by day beside the records.
fn write_bodies(
    dir: &std::path::Path,
    node: &str,
    seq: u64,
    bodies: &[RequestBodies],
) -> std::io::Result<()> {
    let mut by_day: BTreeMap<String, Vec<&RequestBodies>> = BTreeMap::new();
    for body in bodies {
        by_day.entry(day_partition(body.ts)).or_default().push(body);
    }
    for (day, bodies) in by_day {
        let day_dir = dir.join("bodies").join(&day);
        std::fs::create_dir_all(&day_dir)?;
        let path = day_dir.join(format!("{node}-{seq:08}.jsonl.zst"));
        let file = std::fs::File::create(&path)?;
        // Level 6 rather than the 3 used for metadata: bodies are the
        // bulk of what is stored and they are prose, which compresses
        // well enough to be worth the extra CPU on a background thread.
        let mut encoder = zstd::Encoder::new(file, 6)?;
        for body in bodies {
            serde_json::to_writer(&mut encoder, body)?;
            encoder.write_all(b"\n")?;
        }
        encoder.finish()?.sync_all()?;
    }
    Ok(())
}

/// Ship closed partitions to the control-plane store.
///
/// Local disk is per node and dies with the box: config and keys already
/// survive in the store, and spend history not surviving alongside them
/// is the sharpest edge in this design. Shipping the same zstd JSONL that
/// is written locally keeps one format end to end — readable by this
/// gateway, and by DuckDB or Athena directly against the bucket, with no
/// conversion step to get wrong.
///
/// A shipped file is marked with a sibling `.shipped` marker rather than
/// deleted: the local copy still answers reads without a network hop, and
/// retention removes both together. Only files that can no longer change
/// are shipped — the flusher writes each batch to its own sequence file
/// and never reopens one, so any file that exists is already closed.
pub async fn ship_partitions(
    store: &router_store::Store,
    dir: &std::path::Path,
    node: &str,
) -> (usize, usize) {
    let mut shipped = 0;
    let mut failed = 0;
    for kind in ["usage", "rollup", "bodies"] {
        let root = dir.join(kind);
        let Ok(days) = std::fs::read_dir(&root) else {
            continue;
        };
        for day in days.flatten() {
            let day_name = day.file_name().to_string_lossy().into_owned();
            if !day_name.starts_with("dt=") {
                continue;
            }
            let Ok(files) = std::fs::read_dir(day.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                if path.extension().and_then(|e| e.to_str()) != Some("zst") {
                    continue;
                }
                let marker = path.with_extension("zst.shipped");
                if marker.exists() {
                    continue;
                }
                let Ok(body) = std::fs::read(&path) else {
                    failed += 1;
                    continue;
                };
                let name = path.file_name().unwrap_or_default().to_string_lossy();
                let key = format!("{kind}/{day_name}/node={node}/{name}");
                match store.put_blob(&key, body).await {
                    Ok(()) => {
                        let _ = std::fs::write(&marker, b"");
                        shipped += 1;
                    }
                    Err(err) => {
                        tracing::debug!(%err, key, "usage upload failed; will retry");
                        failed += 1;
                    }
                }
            }
        }
    }
    (shipped, failed)
}

/// Rollup rows for a day range, read from every node's shipped objects.
///
/// The fleet-wide view: a node only ever writes its own traffic, so a
/// console served by one node would otherwise report a fraction of the
/// spend and an operator would have no way to tell. Rollups are small
/// enough that reading the whole window from the store is cheap, and the
/// result is cached briefly so a page of charts is one read, not eight.
pub async fn fleet_rollups(
    store: &router_store::Store,
    days: u32,
    exclude_node: &str,
) -> Vec<RollupRow> {
    let cutoff = day_partition(vkey::unix_now_ms().saturating_sub(days.max(1) as u64 * 86_400_000));
    let Ok(keys) = store.list_blobs("rollup/").await else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for key in keys {
        // rollup/dt=YYYY-MM-DD/node=<id>/<file>
        let mut parts = key.split('/');
        let (Some(_), Some(day), Some(node)) = (parts.next(), parts.next(), parts.next()) else {
            continue;
        };
        if day < cutoff.as_str() {
            continue;
        }
        // This node's own rows are already read from local disk.
        if node.trim_start_matches("node=") == exclude_node {
            continue;
        }
        let Ok(Some(body)) = store.get_blob(&key).await else {
            continue;
        };
        let Ok(decoder) = zstd::Decoder::new(std::io::Cursor::new(body)) else {
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

/// Day partitions at or after `cutoff`, sorted.
fn partitions_since(dir: &std::path::Path, cutoff: &str) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            // Lexicographic compare works for `dt=YYYY-MM-DD`.
            (name.starts_with("dt=") && name.as_str() >= cutoff).then(|| (name, e.path()))
        })
        .collect();
    found.sort();
    found
}

/// Every rollup row in one day partition.
fn read_rollup_dir(path: &std::path::Path) -> Vec<RollupRow> {
    let Ok(files) = std::fs::read_dir(path) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for file in files.flatten() {
        let Ok(handle) = std::fs::File::open(file.path()) else {
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

/// Rollups outlive the records they summarise.
///
/// They cost about a thousandth as much per day, and the thing an
/// operator looks back on months later is spend, not individual
/// requests. Four times the raw window is free in practice and turns a
/// 30-day gateway into one that can still draw a quarter's cost curve.
const ROLLUP_RETENTION_FACTOR: u32 = 4;

fn prune(dir: &std::path::Path, retention_days: u32, body_retention_days: u32) {
    prune_partitions(&dir.join("usage"), retention_days, "usage");
    prune_partitions(&dir.join("bodies"), body_retention_days, "bodies");
    prune_partitions(
        &dir.join("rollup"),
        retention_days.saturating_mul(ROLLUP_RETENTION_FACTOR),
        "rollup",
    );
}

fn prune_partitions(usage_dir: &std::path::Path, retention_days: u32, label: &str) {
    let Ok(entries) = std::fs::read_dir(usage_dir) else {
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
                tracing::warn!(%err, partition = name, kind = label, "retention prune failed");
            } else {
                tracing::info!(
                    partition = name,
                    kind = label,
                    "pruned partition past retention"
                );
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

/// Fetch and parse a public price catalog.
///
/// Goes out through the gateway's own HTTP client rather than a second
/// one: same TLS stack, same timeouts, same proxy environment as every
/// upstream call, so a network that can reach providers can reach this.
pub async fn fetch_catalog(
    client: &crate::upstream::UpstreamClient,
    url: &str,
) -> Result<BTreeMap<String, Price>, String> {
    let request = http::Request::builder()
        .method("GET")
        .uri(url)
        .header(http::header::USER_AGENT, "rapid-router")
        .body(axum::body::Body::empty())
        .map_err(|e| format!("invalid catalog url: {e}"))?;
    let response = client
        .send("price-catalog", request, std::time::Duration::from_secs(20))
        .await
        .map_err(|e| format!("could not reach the price catalog: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("price catalog returned {}", response.status()));
    }
    // Catalogs are a couple of megabytes; the cap is a sanity bound, not
    // a tuning knob.
    let body = axum::body::to_bytes(
        axum::body::Body::new(response.into_body()),
        32 * 1024 * 1024,
    )
    .await
    .map_err(|e| format!("could not read the price catalog: {e}"))?;
    Pricing::parse_catalog(&body)
}

/// Write raw records the way the flusher does. Exposed for the scale
/// test, which needs a corpus that is byte-identical to production's.
#[doc(hidden)]
pub fn write_batch_for_test(dir: &std::path::Path, node: &str, seq: u64, batch: &[UsageRecord]) {
    write_batch(dir, node, seq, batch).expect("test corpus writes");
}

/// Write the rollups for a batch, as the flusher does.
#[doc(hidden)]
pub fn write_rollups_for_test(dir: &std::path::Path, node: &str, seq: u64, batch: &[UsageRecord]) {
    write_rollups(dir, node, seq, &roll_up(batch)).expect("test corpus writes");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(
        ts: u64,
        provider: &str,
        model: &str,
        vkey: Option<&str>,
        status: u16,
    ) -> UsageRecord {
        UsageRecord {
            ts,
            request_id: "r".into(),
            endpoint: "/v1/chat/completions".into(),
            requested: model.into(),
            provider: provider.into(),
            model: model.into(),
            vkey: vkey.map(str::to_owned),
            status,
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

    /// The rollup must answer exactly what a full scan of the same
    /// records would, for every grouping and filter the console offers.
    ///
    /// This is the whole safety argument for reading aggregates instead
    /// of records: if the two ever disagree, the console is quietly
    /// wrong about money.
    #[test]
    fn rollups_agree_with_the_records_they_summarise() {
        let now = vkey::unix_now_ms();
        let batch: Vec<UsageRecord> = (0..500)
            .map(|i| {
                let provider = if i % 2 == 0 { "openai" } else { "anthropic" };
                let model = if i % 3 == 0 {
                    "gpt-5"
                } else {
                    "claude-sonnet-4-5"
                };
                let vkey = if i % 5 == 0 { Some("vk_a") } else { None };
                let status = if i % 7 == 0 { 429 } else { 200 };
                // Spread across a few hours so more than one row exists.
                record(now - (i as u64 * 60_000), provider, model, vkey, status)
            })
            .collect();

        let rows = roll_up(&batch);
        assert!(rows.len() > 1, "several hours and dimensions expected");

        // Totals must survive the fold exactly.
        let sum = |f: fn(&RollupRow) -> u64| rows.iter().map(f).sum::<u64>();
        assert_eq!(sum(|r| r.requests), batch.len() as u64);
        assert_eq!(
            sum(|r| r.failed),
            batch.iter().filter(|r| r.status >= 400).count() as u64
        );
        assert_eq!(
            sum(|r| r.cost_micro_usd),
            batch.iter().map(|r| r.cost_micro_usd).sum::<u64>()
        );
        assert_eq!(
            sum(|r| r.input_tokens),
            batch.iter().map(|r| r.input_tokens).sum::<u64>()
        );

        // And a filtered slice must match the records it stands for.
        let filter = HistoryFilter {
            provider: Some("openai".into()),
            model: None,
            vkey: None,
        };
        let rolled: u64 = rows
            .iter()
            .filter(|r| filter.matches_dims(&r.provider, &r.model, r.vkey.as_deref()))
            .map(|r| r.requests)
            .sum();
        let scanned = batch.iter().filter(|r| filter.matches(r)).count() as u64;
        assert_eq!(
            rolled, scanned,
            "a filtered rollup must match a filtered scan"
        );
    }

    /// A rollup file round-trips, and a day covered by rollups is not
    /// also counted from the raw records.
    #[test]
    fn a_day_is_counted_once_even_though_both_forms_exist() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        let now = vkey::unix_now_ms();
        let batch = vec![
            record(now, "openai", "gpt-5", None, 200),
            record(now, "openai", "gpt-5", None, 200),
        ];
        write_batch(&root, "node-a", 0, &batch).unwrap();
        write_rollups(&root, "node-a", 0, &roll_up(&batch)).unwrap();

        let pipeline = UsagePipeline {
            data_dir: Some(root),
            tx: Mutex::new(None),
            agg: Aggregator::new(),
            recent: Mutex::new(VecDeque::new()),
            fleet: Mutex::new(Vec::new()),
            dropped: AtomicU64::new(0),
            per_key_metrics: false,
            key_label_cap: 0,
            body_tx: Mutex::new(None),
            capture: BodyCapture::Off,
            body_limit: 0,
        };
        let history = pipeline.history(2, "model", &HistoryFilter::default());
        let total: u64 = history
            .values()
            .flat_map(|days| days.iter().map(|d| d.requests))
            .sum();
        assert_eq!(
            total, 2,
            "the rollup answered; the raw scan must not double it"
        );
    }

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
        prune(dir.path(), 30, 30);
        let partitions: Vec<String> = std::fs::read_dir(dir.path().join("usage"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(partitions.len(), 1);
        assert_eq!(partitions[0], day_partition(now));
    }
}
