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

use crate::histogram::LatencyHistogram;
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
    /// A short, human-readable excerpt of what was asked — the first user
    /// turn, or the system prompt when there is no user turn.
    ///
    /// Extracted once, here, rather than by the console: the request list
    /// carries no bodies (a page of a hundred would be megabytes nobody
    /// reads), so a log table can only show the prompt if the record
    /// itself carries one. Absent on records written before this shipped.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// Caller-supplied dimensions — which workflow, chart, agent and
    /// pipeline stage this request belongs to.
    ///
    /// A map rather than named columns because the vocabulary is the
    /// caller's, not the gateway's: it is bounded by
    /// [`UsageConfig::trace_keys`], so the cardinality risk a free map
    /// would carry is answered by config instead of by the type.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub meta: BTreeMap<String, String>,
    /// Why a failed request failed, beyond its HTTP status.
    ///
    /// The status alone loses the distinction that matters when you are
    /// paged: `429` is both "slow down" and "this account is out of
    /// quota", and `502` says nothing at all about which upstream
    /// condition produced it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_class: Option<String>,
    /// The account (`provider/key`) that actually served this request.
    ///
    /// `provider` says *what* served it; under an account pool, "which
    /// seat is burning the pool" is a different question and this is the
    /// only field that answers it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat: Option<String>,
    /// Milliseconds to the first response byte.
    ///
    /// For a stream this is the number the caller actually feels;
    /// `latency_ms` on a long generation describes the tail, not the
    /// wait.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttft_ms: Option<u64>,
    /// Milliseconds between the caller's source event and this request.
    ///
    /// Derived from the caller's `event_create_ts`, so it measures the
    /// queue *in front of* the gateway — backlog that no gateway-side
    /// latency number can see.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_lag_ms: Option<u64>,
}

/// How much of the prompt the record keeps. Enough for a table cell to be
/// useful and a tooltip to be worth reading; short enough that a million
/// records a day does not become a second body store.
const PROMPT_PREVIEW_CHARS: usize = 240;

/// Pull a readable excerpt out of a request body, whatever dialect it is.
///
/// The three inbound shapes disagree about where the prompt lives:
/// Chat Completions puts everything in `messages`, the Responses API in
/// `input` (with the system prompt hoisted to `instructions`), and
/// Anthropic keeps `system` separate from `messages`. All three are read
/// here so the console does not have to guess per row.
///
/// Prefers the first *user* turn, because that is what the request is
/// about; falls back to the system prompt, which is better than nothing
/// for an embeddings or tool-only call.
pub fn prompt_preview(body: &str) -> Option<String> {
    prompt_preview_of(&serde_json::from_str::<Value>(body).ok()?)
}

/// The same, from a body that has already been parsed.
///
/// Every record reads two things out of one request body — this preview
/// and the caller's dimensions. Parsing it once for both keeps the cost
/// proportional to the body rather than to how many things want a look
/// at it, which matters when a chart body runs to a quarter megabyte.
pub fn prompt_preview_of(value: &Value) -> Option<String> {
    let user = first_turn_text(value, "user");

    let text = user.or_else(|| {
        // No user turn: fall back to whatever framing the caller set.
        value
            .get("instructions")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or_else(|| system_text(value))
            .or_else(|| first_turn_text(value, "system"))
    })?;
    let text = collapse_whitespace(&text);
    if text.is_empty() {
        return None;
    }
    Some(truncate_chars(&text, PROMPT_PREVIEW_CHARS))
}

/// Anthropic keeps the system prompt out of `messages`, as either a bare
/// string or an array of blocks.
fn system_text(value: &Value) -> Option<String> {
    match value.get("system")? {
        Value::String(text) => Some(text.clone()),
        Value::Array(blocks) => {
            let joined = blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join(" ");
            (!joined.is_empty()).then_some(joined)
        }
        _ => None,
    }
}

/// The text of the first turn with the given role, across `messages`
/// (Chat Completions, Anthropic) and `input` (Responses).
fn first_turn_text(value: &Value, role: &str) -> Option<String> {
    let turns = value
        .get("messages")
        .or_else(|| value.get("input"))
        .and_then(Value::as_array)?;
    for turn in turns {
        // A Responses `input` array also carries non-message items
        // (function_call, reasoning); those have no role and are skipped.
        if turn.get("role").and_then(Value::as_str) != Some(role) {
            continue;
        }
        let text = match turn.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(parts)) => content_text(parts),
            _ => continue,
        };
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    None
}

/// Text out of a content-part array, naming the attachments it passes.
///
/// An attachment is described rather than skipped: a row reading
/// "[image] what is wrong with this chart?" tells you far more about the
/// request than the text alone, and a prompt that is *only* an image
/// would otherwise look empty.
fn content_text(parts: &[Value]) -> String {
    let mut out: Vec<String> = Vec::new();
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") | Some("input_text") | Some("output_text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push(text.to_owned());
                }
            }
            Some("image") | Some("image_url") | Some("input_image") => {
                out.push("[image]".to_owned())
            }
            Some("document") | Some("file") | Some("input_file") => {
                out.push("[document]".to_owned())
            }
            _ => {}
        }
    }
    out.join(" ")
}

/// Newlines and runs of spaces become single spaces: the preview lands in
/// a table cell, where a multi-line prompt would break the row height.
fn collapse_whitespace(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate on a character boundary, not a byte one — a prompt is as
/// likely to be Japanese or to contain an emoji as not.
fn truncate_chars(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Caller-supplied dimensions
// ---------------------------------------------------------------------------

/// Where a caller's `event_create_ts` stops being believable as a
/// timestamp. Callers send it as a string in whichever unit their
/// language handed them, so the reader has to guess seconds from
/// milliseconds and reject the rest.
const MIN_PLAUSIBLE_MS: u64 = 1_262_304_000_000; // 2010-01-01

/// The caller-supplied context lifted out of one request body.
#[derive(Debug, Default)]
pub struct TraceInfo {
    /// Allowed dimensions, by canonical name.
    pub dims: BTreeMap<String, String>,
    /// The caller's source-event timestamp, kept out of `dims` because it
    /// is a measurement rather than something anyone filters on.
    pub event_create_ts: Option<String>,
}

/// Lift the caller's dimensions out of a request body.
///
/// Three shapes reach this gateway and all three are read here, because
/// which one arrives is a property of the client library rather than of
/// the request:
///
/// * `metadata.{org_id,chart_id,…}` — flat, sent by clients talking to
///   the gateway directly.
/// * `metadata.trace_metadata.{…}` — nested, the Langfuse shape that
///   LiteLLM-based clients emit.
/// * `X-Org-Id` / `X-Chart-Id` headers — a fallback for the keys a body
///   omits, merged into the result by the caller of this function.
///
/// Nested wins over flat where both somehow appear, and the body wins
/// over headers: a header is the fallback for something the body did not
/// say, so letting it overwrite a value the body *did* say would invert
/// the contract clients were written against.
///
/// The body is parsed once for both outputs. A body that is not JSON, or
/// that carries no `metadata`, yields an empty result rather than an
/// error — attribution is a side channel and must never fail a request.
pub fn trace_info(body: &str, allow: &BTreeSet<String>, value_chars: usize) -> TraceInfo {
    match serde_json::from_str::<Value>(body) {
        Ok(value) => trace_info_of(&value, allow, value_chars),
        Err(_) => TraceInfo::default(),
    }
}

/// The same, from a body that has already been parsed.
pub fn trace_info_of(value: &Value, allow: &BTreeSet<String>, value_chars: usize) -> TraceInfo {
    let mut info = TraceInfo::default();
    let Some(metadata) = value.get("metadata").and_then(Value::as_object) else {
        return info;
    };
    // `tags` is skipped deliberately rather than missed: it restates the
    // structured keys as `orgId:…` strings alongside free-form labels,
    // and storing both spellings of one fact makes the log wider without
    // making it more answerable.
    collect_dims(metadata, allow, value_chars, &mut info.dims);
    info.event_create_ts = scalar(metadata.get("event_create_ts"));
    if let Some(nested) = metadata.get("trace_metadata").and_then(Value::as_object) {
        collect_dims(nested, allow, value_chars, &mut info.dims);
        info.event_create_ts = scalar(nested.get("event_create_ts")).or(info.event_create_ts);
    }
    info
}

/// Take the allowed keys out of one metadata object, canonicalising
/// names and rendering scalar values.
fn collect_dims(
    object: &serde_json::Map<String, Value>,
    allow: &BTreeSet<String>,
    value_chars: usize,
    out: &mut BTreeMap<String, String>,
) {
    if allow.is_empty() {
        return;
    }
    for (key, value) in object {
        let canonical = router_core::config::canonical_trace_key(key.as_str());
        if !allow.contains(canonical) {
            continue;
        }
        // Only scalars: an object or array under an allowed key is a
        // caller mistake, and flattening one would put unbounded text
        // into a field the console renders as a filter chip.
        let Some(rendered) = scalar(Some(value)) else {
            continue;
        };
        out.insert(canonical.to_owned(), truncate_chars(&rendered, value_chars));
    }
}

/// A JSON scalar as a trimmed string; `None` for containers, nulls, and
/// values that are empty once trimmed.
fn scalar(value: Option<&Value>) -> Option<String> {
    let rendered = match value? {
        Value::String(text) => text.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        _ => return None,
    };
    (!rendered.is_empty()).then_some(rendered)
}

/// Milliseconds a request waited in the caller's queue before it reached
/// the gateway, from the `event_create_ts` the caller stamped on it.
///
/// Accepts seconds or milliseconds — callers send whichever their
/// language's clock returned — and discards anything outside a plausible
/// window, since a misparsed unit yields a lag of decades rather than an
/// obvious error.
fn queue_lag_ms(created: Option<&str>, now_ms: u64) -> Option<u64> {
    let parsed: u64 = created?.trim().parse().ok()?;
    // Ten digits is seconds, thirteen is milliseconds; scale the short
    // one rather than rejecting it. Anything before 2010 is neither.
    let created_ms = if parsed < MIN_PLAUSIBLE_MS / 1000 {
        return None;
    } else if parsed < MIN_PLAUSIBLE_MS {
        parsed.checked_mul(1000)?
    } else {
        parsed
    };
    // Clock skew between the caller's box and this one; a negative lag
    // is noise, not a measurement.
    now_ms.checked_sub(created_ms)
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

/// What to call a dimension the request never reached.
///
/// A request that failed before a provider was chosen has no model and
/// no provider. It is still a request, and grouping it under an empty
/// name would hide exactly the failures worth seeing.
fn model_name(model: &str) -> &str {
    if model.is_empty() { "(none)" } else { model }
}

/// Latency accumulated per model per bucket.
///
/// The one shape every source can produce — the minute aggregate, hourly
/// rollups and raw records alike — and the only one an average composes
/// from: means do not sum, but a count and a sum of milliseconds do, and
/// the division happens once at the end.
#[derive(Debug, Default)]
struct LatencyGrid {
    /// model -> bucket start (unix seconds) -> (requests, latency sum).
    cells: BTreeMap<String, BTreeMap<u64, (u64, u64)>>,
}

impl LatencyGrid {
    fn add(&mut self, model: &str, bucket: u64, requests: u64, latency_ms_sum: u64) {
        let slot = self
            .cells
            .entry(model.to_owned())
            .or_default()
            .entry(bucket)
            .or_insert((0, 0));
        slot.0 += requests;
        slot.1 += latency_ms_sum;
    }

    /// The busiest `limit` models, biggest first.
    ///
    /// Capped because this draws a line per model: a gateway serving
    /// fifty of them produces a chart nobody can read, and the long tail
    /// is what the per-model table underneath is for.
    fn top(self, limit: usize) -> Vec<ModelLatency> {
        let mut series: Vec<ModelLatency> = self
            .cells
            .into_iter()
            .map(|(name, buckets)| ModelLatency {
                name,
                requests: buckets.values().map(|(requests, _)| requests).sum(),
                points: buckets
                    .into_iter()
                    .map(|(ts, (requests, latency_ms_sum))| LatencyPoint {
                        ts,
                        requests,
                        latency_ms_sum,
                    })
                    .collect(),
            })
            .collect();
        series.sort_by(|a, b| {
            b.requests
                .cmp(&a.requests)
                .then_with(|| a.name.cmp(&b.name))
        });
        series.truncate(limit);
        series
    }
}

/// How many models the per-model latency chart draws.
const LATENCY_SERIES_CAP: usize = 6;

/// One model's latency over time, as counts and sums rather than means.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ModelLatency {
    pub name: String,
    /// Requests behind the whole series, which is what ranks it.
    pub requests: u64,
    pub points: Vec<LatencyPoint>,
}

/// One bucket of one model's latency.
///
/// The average is `latency_ms_sum / requests`, left to the reader so
/// that a chart can re-bucket without averaging averages.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct LatencyPoint {
    pub ts: u64,
    pub requests: u64,
    pub latency_ms_sum: u64,
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
    /// The earliest minute this aggregator can speak for.
    ///
    /// Needed to tell "no traffic in that window" from "this process was
    /// not running then" — an empty slot looks identical either way, and
    /// answering the second with the first is how a restarted gateway
    /// would draw a flat zero across the morning and call it a chart.
    ///
    /// Starts at the minute it was built, since from then on it was
    /// watching and an empty slot really does mean no traffic. Lowered
    /// by any record older than that, which is how a backfilled or
    /// out-of-order record widens what it can answer for.
    since_minute: AtomicU64,
}

impl Aggregator {
    fn new() -> Self {
        Self {
            slots: (0..MINUTES)
                .map(|_| Mutex::new(MinuteSlot::default()))
                .collect(),
            since_minute: AtomicU64::new(vkey::unix_now_ms() / 60_000),
        }
    }

    /// The earliest minute this can answer for.
    ///
    /// Bounded below by when it started *and* by the ring's length: a
    /// process up for a week still only holds the last day.
    fn floor_minute(&self, now_ms: u64) -> Option<u64> {
        let first = self.since_minute.load(Ordering::Relaxed);
        Some(first.max((now_ms / 60_000).saturating_sub(MINUTES as u64 - 1)))
    }

    /// Per-minute totals for a window, re-bucketed and filtered, plus the
    /// same traffic split by model for the latency chart.
    ///
    /// The only source with sub-hour resolution. Rollups are hourly by
    /// design — that is what makes a year affordable — so a chart of the
    /// last hour has to come from here or from raw records, and this
    /// costs a walk over sixty small maps.
    ///
    /// The per-model split comes back from the same walk because it is
    /// the same maps: the cells are keyed by model already, so the only
    /// alternative is walking them twice for one of the two answers.
    fn series(
        &self,
        since_ms: u64,
        until_ms: u64,
        bucket_secs: u64,
        filter: &HistoryFilter,
    ) -> (Vec<UsageBucket>, LatencyGrid) {
        let mut out: BTreeMap<u64, UsageBucket> = BTreeMap::new();
        let mut grid = LatencyGrid::default();
        let bucket_secs = bucket_secs.max(60);
        for minute in (since_ms / 60_000)..=(until_ms / 60_000) {
            let slot = self.slots[(minute as usize) % MINUTES].lock().unwrap();
            if slot.minute != minute {
                continue;
            }
            for (key, cell) in &slot.cells {
                if !filter.matches_dims(&key.provider, &key.model, key.vkey.as_deref()) {
                    continue;
                }
                let bucket = (minute * 60 / bucket_secs) * bucket_secs;
                let point = out.entry(bucket).or_insert(UsageBucket {
                    ts: bucket,
                    ..Default::default()
                });
                point.requests += cell.requests;
                point.failed += cell.errors;
                point.input_tokens += cell.input_tokens;
                point.output_tokens += cell.output_tokens;
                point.cost_micro_usd += cell.cost_micro_usd;
                point.latency_ms_sum += cell.latency_ms_sum;
                grid.add(
                    model_name(&key.model),
                    bucket,
                    cell.requests,
                    cell.latency_ms_sum,
                );
            }
        }
        (out.into_values().collect(), grid)
    }

    fn record(&self, rec: &UsageRecord) {
        let minute = rec.ts / 60_000;
        self.since_minute.fetch_min(minute, Ordering::Relaxed);
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
                "avg_latency_ms": c.latency_ms_sum.checked_div(c.requests).unwrap_or(0),
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

/// How many requests' bodies are kept in memory for the drawer.
///
/// Matched to the metadata ring above, because they answer the same
/// question: a row the operator can see is a row they may click. Past
/// that the index takes over, which is one file read rather than none.
const HOT_BODIES_CAP: usize = RECENT_CAP;

/// The bodies of the most recent requests, evicted oldest-first.
///
/// Bounded by *count* rather than bytes, and safe to be: each body is
/// already capped at `body_limit_bytes` (256 KiB by default) before it
/// reaches here, so the ceiling is that times the cap.
#[derive(Default)]
struct HotBodies {
    by_id: HashMap<String, RequestBodies>,
    order: VecDeque<String>,
}

impl HotBodies {
    fn get(&self, request_id: &str) -> Option<&RequestBodies> {
        self.by_id.get(request_id)
    }

    fn insert(&mut self, bodies: RequestBodies) {
        if self.by_id.contains_key(&bodies.request_id) {
            return;
        }
        while self.order.len() >= HOT_BODIES_CAP {
            if let Some(oldest) = self.order.pop_front() {
                self.by_id.remove(&oldest);
            }
        }
        self.order.push_back(bodies.request_id.clone());
        self.by_id.insert(bodies.request_id.clone(), bodies);
    }
}

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
    /// Hourly rollups for the partitions still being written to, so the
    /// charts never have to open today's thousands of delta files.
    recent_rollups: Arc<Mutex<RecentRollups>>,
    /// The most recently captured bodies, so opening a request from the
    /// live log tail touches no disk at all.
    hot_bodies: Mutex<HotBodies>,
    body_tx: Mutex<Option<mpsc::SyncSender<RequestBodies>>>,
    capture: BodyCapture,
    body_limit: usize,
    /// Which caller metadata keys become dimensions, and how much of
    /// each value is kept.
    trace_keys: BTreeSet<String>,
    trace_value_chars: usize,
}

impl UsagePipeline {
    /// Start the pipeline. With a data dir, a flusher thread writes
    /// batches to disk and prunes retention; without one (pure env/file
    /// setups), aggregation is in-memory only.
    pub fn start(data_dir: Option<PathBuf>, cfg: &UsageConfig, node_id: &str) -> Arc<Self> {
        let history_dir = data_dir.clone();
        let mut body_tx = None;
        let recent_rollups: Arc<Mutex<RecentRollups>> = Arc::default();
        let flusher_recent = recent_rollups.clone();
        let tx = data_dir.map(|dir| {
            let (tx, rx) = mpsc::sync_channel::<UsageRecord>(8192);
            // A shallower queue than the metadata one: bodies are large,
            // and a backlog of them is memory. Dropping a body under
            // pressure costs a debugging view; dropping a record would
            // cost money accounting.
            let (btx, brx) = mpsc::sync_channel::<RequestBodies>(1024);
            body_tx = Some(btx);
            let settings = FlushSettings {
                dir,
                node: node_id.to_owned(),
                interval: cfg.flush_interval,
                retention_days: cfg.retention_days,
                body_retention_days: cfg.body_retention_days,
            };
            std::thread::Builder::new()
                .name("rapid-usage-flush".into())
                .spawn(move || flusher(settings, rx, brx, flusher_recent))
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
            recent_rollups,
            hot_bodies: Mutex::new(HotBodies::default()),
            dropped: AtomicU64::new(0),
            per_key_metrics: cfg.per_key_metrics,
            key_label_cap: 100,
            trace_keys: cfg.trace_keys.clone(),
            trace_value_chars: cfg.trace_value_chars,
        })
    }

    /// Which caller metadata keys this gateway lifts onto records.
    ///
    /// Exposed because the same allowlist governs both sources of a
    /// dimension — the request body and the identity headers — and the
    /// header side is read in the proxy, before a hook exists.
    pub fn trace_keys(&self) -> &BTreeSet<String> {
        &self.trace_keys
    }

    /// Characters kept per dimension value, from config.
    pub fn trace_value_chars(&self) -> usize {
        self.trace_value_chars
    }

    /// Whether the request body is worth materialising at all.
    ///
    /// Broader than "will a body be stored": the prompt preview and the
    /// caller's dimensions are both read out of the request body and
    /// belong on the record whether or not bodies are kept, so a gateway
    /// with `capture_bodies = "off"` still needs the bytes in hand for
    /// the length of one request. Also true in `"errors"` mode, where
    /// whether this body will be stored is not known until the status is.
    pub fn wants_input_body(&self) -> bool {
        self.capture != BodyCapture::Off || !self.trace_keys.is_empty()
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
        let (input, cut_in) = cap_body(input, self.body_limit);
        let (output, cut_out) = cap_body(output, self.body_limit);
        let bodies = RequestBodies {
            request_id: request_id.to_owned(),
            ts,
            input,
            output,
            truncated: cut_in || cut_out,
        };
        // Held in memory first, and whether or not there is anywhere to
        // write it. Readable immediately rather than a flush interval
        // from now — ten seconds is exactly the window in which somebody
        // watching a live log clicks the row that just appeared — and on
        // a gateway with no data directory this is the only copy there
        // will ever be, which is better than none.
        if let Ok(mut hot) = self.hot_bodies.lock() {
            hot.insert(bodies.clone());
        }
        let Ok(guard) = self.body_tx.lock() else {
            return;
        };
        let Some(tx) = guard.as_ref() else {
            return;
        };
        let _ = tx.try_send(bodies);
    }

    /// The stored bodies for one request, if they were captured and are
    /// still inside their retention window.
    ///
    /// Three ways to answer, and the first two are the ones that matter,
    /// because the question is nearly always about a request the
    /// operator can see on screen right now:
    ///
    /// 1. **The hot cache**, holding the bodies most recently written.
    ///    Anything visible in a live log tail is a memory read.
    /// 2. **The day's index**, which says which file holds an id. One
    ///    small read, then one file.
    /// 3. **A scan of the day**, for partitions written before the index
    ///    existed. This is what every lookup used to do: open every body
    ///    file in the partition and decompress it looking for a
    ///    substring — thousands of files and hundreds of megabytes to
    ///    return one record, which is why the drawer took seconds to
    ///    open.
    pub fn bodies_for(&self, request_id: &str, ts: u64) -> Option<RequestBodies> {
        if let Ok(hot) = self.hot_bodies.lock()
            && let Some(bodies) = hot.get(request_id)
        {
            return Some(bodies.clone());
        }
        let dir = self
            .data_dir
            .as_ref()?
            .join("bodies")
            .join(day_partition(ts));
        if let Some(file) = bodies_index_lookup(&dir, request_id)
            && let Some(found) = scan_bodies_file(&dir.join(file), request_id)
        {
            return Some(found);
        }
        let files = std::fs::read_dir(dir).ok()?;
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("zst") {
                continue;
            }
            if let Some(found) = scan_bodies_file(&path, request_id) {
                return Some(found);
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
    /// Dimensions read from request *headers*, before the body is seen.
    ///
    /// Seeded at hook construction because that is where the headers
    /// are; the body's own dimensions are merged over these on
    /// completion, so a header only ever fills a gap.
    pub header_dims: BTreeMap<String, String>,
    /// Set on the failure path, where the gateway knows why it failed.
    pub error_class: Option<&'static str>,
    /// Milliseconds to the first response byte, filled in by the metered
    /// body as it streams.
    pub ttft_ms: Option<u64>,
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
        // One parse of the request body serves both readers below — the
        // prompt preview and the caller's dimensions. A body that is not
        // JSON yields neither, rather than failing the request.
        let parsed = self
            .input_body
            .as_deref()
            .and_then(|body| serde_json::from_str::<Value>(body).ok());
        // Body dimensions win over header ones: a header is the
        // documented fallback for a key the body omitted.
        let trace = parsed
            .as_ref()
            .map(|value| {
                trace_info_of(
                    value,
                    &self.pipeline.trace_keys,
                    self.pipeline.trace_value_chars,
                )
            })
            .unwrap_or_default();
        let mut meta = self.header_dims;
        meta.extend(trace.dims);
        let queue_lag_ms = queue_lag_ms(trace.event_create_ts.as_deref(), now_unix);
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
            // Read from the body we already hold, before it is handed to
            // the capture queue — and independently of whether capture is
            // on at all, so the log table shows a prompt even on a
            // gateway that stores no bodies.
            prompt: parsed.as_ref().and_then(prompt_preview_of),
            meta,
            error_class: self.error_class.map(str::to_owned),
            // `provider/key`, so a seat is readable on its own and sorts
            // with its provider's other seats.
            seat: self
                .seat
                .as_ref()
                .map(|s| format!("{}/{}", s.provider.name, s.key)),
            ttft_ms: self.ttft_ms,
            queue_lag_ms,
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
        ttft_ms: None,
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
    /// Time to the first *non-empty* frame.
    ///
    /// Empty frames are skipped because a stream's opening frames can
    /// carry headers or keep-alive padding with no content; timing those
    /// would report a first token that had not arrived yet.
    ttft_ms: Option<u64>,
}

impl MeteredBody {
    fn observe(&mut self, data: &[u8]) {
        if self.ttft_ms.is_none() && !data.is_empty() {
            self.ttft_ms = self
                .hook
                .as_ref()
                .map(|hook| hook.started.elapsed().as_millis() as u64);
        }
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
        let Some(mut hook) = self.hook.take() else {
            return;
        };
        hook.ttft_ms = self.ttft_ms;
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

pub(crate) fn day_partition(ts_ms: u64) -> String {
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

const DAY_MS: u64 = 86_400_000;

/// Midnight on the oldest day this gateway has anything for.
///
/// Two directory listings, so a window that reaches back before the
/// gateway existed costs two syscalls rather than a step per day of the
/// interval. `None` when there is nothing on disk at all.
fn earliest_partition_ms(data_dir: &std::path::Path) -> Option<u64> {
    let mut oldest: Option<String> = None;
    for kind in ["rollup", "usage"] {
        let Ok(entries) = std::fs::read_dir(data_dir.join(kind)) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("dt=") {
                continue;
            }
            if oldest.as_ref().is_none_or(|current| &name < current) {
                oldest = Some(name);
            }
        }
    }
    day_start_ms(&oldest?)
}

/// `dt=YYYY-MM-DD` back to midnight, unix milliseconds.
///
/// The inverse of [`day_partition`], by search rather than by arithmetic:
/// the forward direction is already written and correct about leap
/// years, and a binary search over 65,536 days costs sixteen calls to it.
fn day_start_ms(day: &str) -> Option<u64> {
    if !day.starts_with("dt=") {
        return None;
    }
    let (mut low, mut high) = (0u64, 65_536u64);
    while low < high {
        let mid = (low + high) / 2;
        if day_partition(mid * DAY_MS).as_str() < day {
            low = mid + 1;
        } else {
            high = mid;
        }
    }
    (day_partition(low * DAY_MS) == day).then_some(low * DAY_MS)
}

/// Midnight on the first of the month containing `ts_ms`.
fn month_start_ms(ts_ms: u64) -> u64 {
    let day = ts_ms / DAY_MS;
    let (_, _, d) = civil(day as i64);
    (day - u64::from(d - 1)) * DAY_MS
}

/// Midnight on the first of the *following* month — the exclusive end,
/// so month spans tile without overlapping.
///
/// Walked a day at a time rather than computed. Calendar arithmetic that
/// has to be right about February is worth thirty-one comparisons
/// against zero file reads.
fn month_end_ms(ts_ms: u64) -> u64 {
    let start = month_start_ms(ts_ms);
    let (year, month, _) = civil((start / DAY_MS) as i64);
    let mut cursor = start + 28 * DAY_MS;
    loop {
        let (y, m, _) = civil((cursor / DAY_MS) as i64);
        if (y, m) != (year, month) {
            return cursor - (cursor % DAY_MS);
        }
        cursor += DAY_MS;
    }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
    /// Upstream calls made to serve these requests. Exceeds `requests`
    /// when retries or failover ran, and the gap is the cost of
    /// unhealthy seats — which is a thing an operator looks back on, so
    /// it belongs in the aggregate and not only in the raw records.
    #[serde(default)]
    pub attempts: u64,
    /// The latency distribution, so a window wider than the live tail
    /// can still report a p95. `latency_ms_sum` stays for the mean and
    /// for rows written before this existed.
    #[serde(default, skip_serializing_if = "LatencyHistogram::is_empty")]
    pub latency: LatencyHistogram,
}

impl RollupRow {
    /// The key two rows must share to be foldable into one.
    fn key(&self) -> (u64, String, String, Option<String>) {
        (
            self.hour_ms,
            self.provider.clone(),
            self.model.clone(),
            self.vkey.clone(),
        )
    }

    /// Fold another row for the same key into this one.
    fn absorb(&mut self, other: &RollupRow) {
        self.requests += other.requests;
        self.failed += other.failed;
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cached_tokens += other.cached_tokens;
        self.cost_micro_usd += other.cost_micro_usd;
        self.latency_ms_sum += other.latency_ms_sum;
        self.attempts += other.attempts;
        self.latency.merge(&other.latency);
    }

    fn empty(hour_ms: u64, provider: &str, model: &str, vkey: Option<&str>) -> Self {
        Self {
            hour_ms,
            provider: provider.to_owned(),
            model: model.to_owned(),
            vkey: vkey.map(str::to_owned),
            requests: 0,
            failed: 0,
            input_tokens: 0,
            output_tokens: 0,
            cached_tokens: 0,
            cost_micro_usd: 0,
            latency_ms_sum: 0,
            attempts: 0,
            latency: LatencyHistogram::default(),
        }
    }
}

/// Fold rows onto their keys, in a stable order.
///
/// The one operation compaction, the monthly tier and the summary
/// endpoints all perform, written once. Rows are pure sums, so folding
/// is associative and the order files are read in cannot change the
/// answer.
pub(crate) fn fold_rows(rows: impl IntoIterator<Item = RollupRow>) -> Vec<RollupRow> {
    let mut merged: BTreeMap<(u64, String, String, Option<String>), RollupRow> = BTreeMap::new();
    for row in rows {
        match merged.entry(row.key()) {
            std::collections::btree_map::Entry::Occupied(mut slot) => slot.get_mut().absorb(&row),
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(row);
            }
        }
    }
    merged.into_values().collect()
}

const HOUR_MS: u64 = 3_600_000;

/// Fold a batch of records into hour rows.
pub(crate) fn roll_up(batch: &[UsageRecord]) -> Vec<RollupRow> {
    let mut rows: BTreeMap<(u64, String, String, Option<String>), RollupRow> = BTreeMap::new();
    for rec in batch {
        let hour_ms = rec.ts - (rec.ts % HOUR_MS);
        let key = (
            hour_ms,
            rec.provider.clone(),
            rec.model.clone(),
            rec.vkey.clone(),
        );
        let row = rows.entry(key).or_insert_with(|| {
            RollupRow::empty(hour_ms, &rec.provider, &rec.model, rec.vkey.as_deref())
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
        row.attempts += u64::from(rec.attempts);
        row.latency.record(rec.latency_ms);
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

/// Everything the flusher thread needs that is not a channel: where to
/// write, under what name, and how long to keep it.
struct FlushSettings {
    dir: PathBuf,
    node: String,
    interval: Duration,
    retention_days: u32,
    body_retention_days: u32,
}

fn flusher(
    settings: FlushSettings,
    rx: mpsc::Receiver<UsageRecord>,
    bodies_rx: mpsc::Receiver<RequestBodies>,
    recent: Arc<Mutex<RecentRollups>>,
) {
    let FlushSettings {
        dir,
        node,
        interval,
        retention_days,
        body_retention_days,
    } = settings;
    let mut seq: u64 = 0;
    // Far enough in the past that the first loop runs maintenance
    // immediately: a gateway that has just restarted is exactly when the
    // caches are most likely to be stale.
    let mut last_maintenance = std::time::Instant::now() - MAINTENANCE_INTERVAL;
    // What this process inherited on disk. Read once, here rather than in
    // `start`, so booting is not delayed by it.
    seed_recent_rollups(&dir, &recent);
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
            let rows = roll_up(&batch);
            // Kept before they are written, not after: a failed write
            // costs durability, and serving the charts from memory
            // anyway is strictly better than also losing the reading.
            if let Ok(mut recent) = recent.lock() {
                recent.absorb(&rows);
            }
            if let Err(err) = write_rollups(&dir, &node, seq, &rows) {
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
        if last_maintenance.elapsed() >= MAINTENANCE_INTERVAL {
            last_maintenance = std::time::Instant::now();
            let now = vkey::unix_now_ms();
            prune(&dir, retention_days, body_retention_days);
            // Built here, on the flusher's own thread, and never on a
            // request: a console page load must be able to *find* a
            // cache, never to pay for building one.
            crate::rollup_cache::refresh(&dir, &day_partition(now), &month_partition(now));
            if let Ok(mut recent) = recent.lock() {
                recent.evict_before(now.saturating_sub(RECENT_ROLLUP_SPAN_MS));
            }
        }
    }
}

/// How often the flusher prunes retention and rebuilds the read caches.
const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(300);

/// Read back the hours this process did not record itself.
///
/// A restart leaves today's rollups on disk and nothing in memory, so
/// without this the first five minutes of every deploy would report a
/// day that started when the process did. Two day partitions at most,
/// and it happens once.
fn seed_recent_rollups(dir: &std::path::Path, recent: &Mutex<RecentRollups>) {
    let now = vkey::unix_now_ms();
    let mut rows = Vec::new();
    for back in 0..=1 {
        let day = day_partition(now.saturating_sub(back * 86_400_000));
        rows.extend(crate::rollup_cache::hourly_rows(dir, &day));
    }
    if let Ok(mut recent) = recent.lock() {
        // Absorb rather than replace: the flusher may already have
        // written a batch of its own while this was reading.
        recent.absorb(&rows);
        recent.evict_before(now.saturating_sub(RECENT_ROLLUP_SPAN_MS));
        recent.seeded = true;
    }
}

/// `ym=YYYY-MM` for a timestamp, matching the monthly cache partitions.
fn month_partition(ts_ms: u64) -> String {
    let day = day_partition(ts_ms);
    format!("ym={}", &day.trim_start_matches("dt=")[..7])
}

/// Hourly rollup rows for the recent past, held in memory.
///
/// The tail the on-disk caches cannot cover. Today's partition is still
/// being appended to, so it has no cache, and reading it means opening
/// every delta written since midnight — up to 8,640 files, which is
/// exactly the cost the caches exist to remove. The flusher already has
/// these rows in hand as it writes them, so rather than reading them
/// back it simply keeps them.
///
/// Bounded by hours, not by traffic: two days of hours times the
/// provider/model/key combinations actually served. A gateway with a
/// pathological number of distinct keys is bounded by the same
/// cardinality cap the aggregator applies, since both are fed from the
/// same records.
#[derive(Default)]
pub(crate) struct RecentRollups {
    rows: BTreeMap<(u64, String, String, Option<String>), RollupRow>,
    /// True once the flusher has read back whatever was on disk before
    /// this process started. Until then these rows are only what *this*
    /// process recorded, and a reader must not mistake that for the
    /// whole of today.
    seeded: bool,
}

/// How far back the in-memory tail reaches.
///
/// Two days, so "today" is covered however close to midnight the
/// question is asked, and yesterday stays available for the window
/// between midnight and the maintenance tick that caches it.
const RECENT_ROLLUP_SPAN_MS: u64 = 2 * 86_400_000;

impl RecentRollups {
    fn absorb(&mut self, rows: &[RollupRow]) {
        for row in rows {
            match self.rows.entry(row.key()) {
                std::collections::btree_map::Entry::Occupied(mut slot) => {
                    slot.get_mut().absorb(row)
                }
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(row.clone());
                }
            }
        }
    }

    /// Drop hours that the on-disk caches now cover.
    fn evict_before(&mut self, floor_ms: u64) {
        self.rows.retain(|(hour_ms, ..), _| *hour_ms >= floor_ms);
    }

    /// The earliest hour these rows can answer for, or `None` when they
    /// cannot be trusted as complete yet.
    fn floor_ms(&self, now_ms: u64) -> Option<u64> {
        self.seeded
            .then(|| now_ms.saturating_sub(RECENT_ROLLUP_SPAN_MS))
    }
}

/// Record-level constraints for a history read. Empty fields constrain
/// nothing.
#[derive(Debug, Default)]
pub struct HistoryFilter {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub vkey: Option<String>,
    /// Caller dimensions that must all match — `workflow_id=X` *and*
    /// `stage=Y`. Conjunctive because that is what narrowing a log means;
    /// a disjunction over two different workflows is two queries.
    pub meta: Vec<(String, String)>,
}

impl HistoryFilter {
    /// The same constraints applied to a rollup row's dimensions.
    ///
    /// Rollup rows carry only the aggregate dimensions, so a query that
    /// constrains a caller dimension cannot be served from them; callers
    /// check [`Self::needs_records`] before reaching for a rollup.
    fn matches_dims(&self, provider: &str, model: &str, vkey: Option<&str>) -> bool {
        self.provider.as_deref().is_none_or(|p| provider == p)
            && self.model.as_deref().is_none_or(|m| model == m)
            && self.vkey.as_deref().is_none_or(|k| vkey == Some(k))
    }

    /// Whether this filter can only be answered from raw records.
    ///
    /// Hourly rollups are keyed by provider/model/key alone. Answering a
    /// caller-dimension filter from them would silently ignore that
    /// filter and report the *unfiltered* total, which is worse than
    /// being slower — so the read falls back to scanning records.
    pub fn needs_records(&self) -> bool {
        !self.meta.is_empty()
    }

    fn matches(&self, rec: &UsageRecord) -> bool {
        self.provider.as_deref().is_none_or(|p| rec.provider == p)
            && self.model.as_deref().is_none_or(|m| rec.model == m)
            && self
                .vkey
                .as_deref()
                .is_none_or(|k| rec.vkey.as_deref() == Some(k))
            && self
                .meta
                .iter()
                .all(|(key, want)| rec.meta.get(key).is_some_and(|got| got == want))
    }
}

/// One day's totals, optionally split by a dimension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct DayBucket {
    pub day: String,
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    /// Summed, not averaged, for the same reason the rollup row sums it:
    /// means do not compose, so the mean is `latency_ms_sum / requests`
    /// taken once by whoever draws it.
    #[serde(default)]
    pub latency_ms_sum: u64,
}

/// Daily series for a window, and the latency figures a day bucket
/// cannot carry.
///
/// Percentiles do not sum, so there is no honest per-day p95 to put in a
/// `DayBucket` that a reader could then total up. They are computed once
/// over the same rows the buckets were folded from, and describe the
/// whole window.
#[derive(Debug, Default, Clone, Serialize)]
pub struct History {
    /// `by` -> series name -> days, where `by` is `""` (the total),
    /// `"provider"`, `"model"` or `"key"`.
    pub data: BTreeMap<String, BTreeMap<String, Vec<DayBucket>>>,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
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
            recent_rollups: Arc::default(),
            hot_bodies: Mutex::new(HotBodies::default()),
            dropped: AtomicU64::new(0),
            per_key_metrics: false,
            key_label_cap: 0,
            body_tx: Mutex::new(None),
            capture: BodyCapture::Off,
            body_limit: 0,
            trace_keys: router_core::config::DEFAULT_TRACE_KEYS
                .iter()
                .map(|k| (*k).to_owned())
                .collect(),
            trace_value_chars: 128,
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
                batch.sort_by_key(|rec| std::cmp::Reverse(rec.ts));
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

    /// Rollup rows covering a window, from the cheapest tier that still
    /// has the resolution the caller needs.
    ///
    /// This is the whole point of the rollup work: read cost is
    /// proportional to the window's *resolution*, never to the traffic
    /// inside it. A year at daily resolution is about twelve files. A
    /// week at hourly resolution is seven. Neither depends on whether the
    /// gateway served a thousand requests or a billion.
    ///
    /// Three tiers, coarsest first:
    ///
    /// 1. **Memory** for hours the flusher is still writing, so today
    ///    costs nothing at all.
    /// 2. **A month cache** for whole calendar months inside the window,
    ///    when day resolution is enough. Anything wider than about a
    ///    month is bucketed by day by the time it reaches a chart, so
    ///    hourly detail there would be twenty-four times the reading for
    ///    none of the answer.
    /// 3. **A day cache** otherwise, one file per day.
    ///
    /// Rows are included when their bucket *overlaps* the window, so the
    /// edges are hour-aligned (or day-aligned, at month resolution)
    /// rather than exact to the millisecond. That is the trade rollups
    /// are: the raw path this replaces was exact but capped, and reported
    /// a silent floor on any window with real traffic in it. An hour of
    /// slop at each edge of a seven-day window is a better answer than a
    /// confident number that stopped counting at two hundred thousand.
    fn rollups_for_window(&self, since_ms: u64, until_ms: u64, hourly: bool) -> Vec<RollupRow> {
        let Some(root) = self.data_dir.as_deref() else {
            return Vec::new();
        };
        let now = vkey::unix_now_ms();
        // Callers pass the window the operator selected, which can reach
        // back further than anything was ever written — `0` and
        // `u64::MAX` are both legal and both arrive in practice. The walk
        // below is one step per day, so an unclamped window is not merely
        // wasteful, it does not terminate in any useful time. Bound it by
        // what exists.
        let until_ms = until_ms.min(now);
        let since_ms = since_ms.max(earliest_partition_ms(root).unwrap_or(now));
        if since_ms > until_ms {
            return Vec::new();
        }
        let mut rows = Vec::new();

        // The in-memory tail first: it also tells the disk walk where to
        // stop, so the two can never both count the same hour.
        let memory_floor = {
            let recent = self.recent_rollups.lock().ok();
            let floor = recent.as_ref().and_then(|r| r.floor_ms(now));
            if let (Some(recent), Some(floor)) = (recent, floor) {
                rows.extend(
                    recent
                        .rows
                        .values()
                        .filter(|row| row.hour_ms >= floor.max(since_ms.saturating_sub(HOUR_MS)))
                        .cloned(),
                );
            }
            floor
        };

        // Where the memory tier takes over. Days at or after this are
        // already in `rows` and must not be read from disk as well.
        let disk_ceiling_ms = memory_floor.map(|ms| ms - (ms % DAY_MS));

        let mut cursor = since_ms - (since_ms % DAY_MS);
        while cursor <= until_ms {
            if disk_ceiling_ms.is_some_and(|ceiling| cursor >= ceiling) {
                break;
            }
            // A whole calendar month inside the window, at a resolution
            // the month cache can serve: one file instead of thirty.
            if !hourly && cursor == month_start_ms(cursor) {
                let month_end = month_end_ms(cursor);
                let fits = month_end <= until_ms.saturating_add(1)
                    && disk_ceiling_ms.is_none_or(|ceiling| month_end <= ceiling);
                if fits
                    && let Some(monthly) =
                        crate::rollup_cache::monthly_rows(root, &month_partition(cursor))
                {
                    rows.extend(monthly);
                    cursor = month_end;
                    continue;
                }
            }
            rows.extend(crate::rollup_cache::hourly_rows(
                root,
                &day_partition(cursor),
            ));
            cursor += DAY_MS;
        }

        // Other nodes' rows for the same window. Additive, never
        // duplicative: a row is written by exactly the node that served
        // the traffic.
        if let Ok(fleet) = self.fleet.lock() {
            rows.extend(fleet.iter().cloned());
        }

        // Tiers arrive at different resolutions — memory and day caches
        // in hours, month caches in days. Normalising before the window
        // filter is what lets one rule admit all of them, and folding
        // also merges the duplicate keys that three sources inevitably
        // produce.
        let width = if hourly { HOUR_MS } else { DAY_MS };
        let mut rows = if hourly {
            fold_rows(rows)
        } else {
            crate::rollup_cache::to_daily(rows)
        };
        rows.retain(|row| row.hour_ms <= until_ms && row.hour_ms.saturating_add(width) > since_ms);
        rows
    }

    /// Replace the cached view of what other nodes have recorded.
    pub fn set_fleet_rollups(&self, rows: Vec<RollupRow>) {
        if let Ok(mut fleet) = self.fleet.lock() {
            *fleet = rows;
        }
    }

    /// Daily series for the console, one grouping at a time.
    ///
    /// Kept because the admin API still exposes it; the console asks for
    /// [`Self::history_all`] instead, which produces every grouping from
    /// one read.
    pub fn history(
        &self,
        days: u32,
        by: &str,
        filter: &HistoryFilter,
    ) -> BTreeMap<String, Vec<DayBucket>> {
        self.history_all(days, filter)
            .data
            .remove(by)
            .unwrap_or_default()
    }

    /// Every grouping the console draws, from one walk of the rollups.
    ///
    /// The Usage and Cost pages each asked for three: by model, by key
    /// and by provider. That was three identical reads of the same
    /// window, thrown at the gateway together on every page load and
    /// again every time traffic nudged the refresh. The rows carry all
    /// three dimensions, so the split is arithmetic over rows already in
    /// hand — the read is the expensive part, and it is the same read
    /// for all of them.
    ///
    /// Returns the groupings plus the window's latency percentiles; see
    /// [`History`].
    /// Caller dimensions are not answerable here. This reads rollups
    /// and nothing else — the record-scan path this once fell back to is
    /// gone, which is what makes a year cost what a day did — so
    /// `/history` does not accept `meta.*` terms at all rather than
    /// quietly returning totals that ignore them.
    pub fn history_all(&self, days: u32, filter: &HistoryFilter) -> History {
        if self.data_dir.is_none() {
            return History::default();
        }
        let now = vkey::unix_now_ms();
        let since = now.saturating_sub(days.max(1) as u64 * DAY_MS);
        let rows = self.rollups_for_window(since, now, false);

        let mut out: BTreeMap<String, BTreeMap<String, BTreeMap<String, DayBucket>>> =
            BTreeMap::new();
        let mut latency = LatencyHistogram::default();
        let mut latency_sum = 0u64;
        let mut requests = 0u64;
        for row in rows {
            if !filter.matches_dims(&row.provider, &row.model, row.vkey.as_deref()) {
                continue;
            }
            latency.merge(&row.latency);
            latency_sum += row.latency_ms_sum;
            requests += row.requests;
            let day = day_partition(row.hour_ms)
                .trim_start_matches("dt=")
                .to_owned();
            for (by, series) in [
                ("", "total".to_owned()),
                ("provider", row.provider.clone()),
                ("model", row.model.clone()),
                ("key", row.vkey.clone().unwrap_or_else(|| "(none)".into())),
            ] {
                let bucket = out
                    .entry(by.to_owned())
                    .or_default()
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
                bucket.latency_ms_sum += row.latency_ms_sum;
            }
        }
        History {
            data: out
                .into_iter()
                .map(|(by, groupings)| {
                    (
                        by,
                        groupings
                            .into_iter()
                            .map(|(series, days)| (series, days.into_values().collect()))
                            .collect(),
                    )
                })
                .collect(),
            // Rows written before histograms existed carry only a sum,
            // and the mean is the only percentile a sum can honestly
            // supply — better than a confident zero for a window whose
            // older half predates the upgrade.
            p50_latency_ms: if latency.is_empty() {
                latency_sum.checked_div(requests).unwrap_or(0)
            } else {
                latency.percentile(50)
            },
            p95_latency_ms: if latency.is_empty() {
                latency_sum.checked_div(requests).unwrap_or(0)
            } else {
                latency.percentile(95)
            },
        }
    }
}

/// Totals for every request matching a range and filter, not just the
/// page being displayed.
///
/// The console's header used to be computed from the rows it had in hand,
/// which meant "Requests shown: 200" on a window holding forty thousand —
/// a number that looked authoritative and answered a question nobody had
/// asked. These are the real totals for the window.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct RequestsSummary {
    /// Client requests matching the range and filters.
    pub requests: u64,
    /// Of those, the ones that failed (HTTP >= 400).
    pub errors: u64,
    /// Upstream calls made to serve them. Higher than `requests` when
    /// retries or failover were involved, and the gap is the interesting
    /// part: it is the cost of unhealthy seats.
    pub attempts: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micro_usd: u64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    /// True when the scan hit its ceiling, so the totals are a floor
    /// rather than the whole window. Never silently exact-looking.
    pub capped: bool,
}

/// The most records a summary will scan. A month-wide window on a busy
/// gateway is millions of rows, and an admin endpoint is not allowed to
/// spend a minute of CPU on one page load.
const SUMMARY_SCAN_CAP: usize = 200_000;

impl UsagePipeline {
    /// Aggregate every matching record in the window.
    ///
    /// Rollups answer this whenever they can, for the same reason they
    /// answer [`Self::usage_summary`]: this header is drawn on every
    /// Logs page load and again on every refresh, and it used to walk up
    /// to two hundred thousand records to produce eight numbers.
    pub fn summary(
        &self,
        since_ms: u64,
        until_ms: u64,
        filter: &HistoryFilter,
        errors_only: bool,
    ) -> RequestsSummary {
        // A caller-dimension filter cannot be served from rollups —
        // their rows carry provider, model and key and nothing else — so
        // reading them here would drop the filter and report the
        // *unfiltered* total as the answer. The record scan below is
        // slower and is the only one that answers the question asked.
        if !errors_only && self.data_dir.is_some() && !filter.needs_records() {
            let wide = self.usage_summary_from_rollups(
                since_ms,
                until_ms,
                filter,
                bucket_width_secs(until_ms.saturating_sub(since_ms)),
            );
            return RequestsSummary {
                requests: wide.requests,
                errors: wide.errors,
                attempts: wide.attempts,
                input_tokens: wide.input_tokens,
                output_tokens: wide.output_tokens,
                cached_tokens: wide.cached_tokens,
                cost_micro_usd: wide.cost_micro_usd,
                p50_latency_ms: wide.p50_latency_ms,
                p95_latency_ms: wide.p95_latency_ms,
                capped: false,
            };
        }
        let records = self.collect_page(
            SUMMARY_SCAN_CAP + 1,
            since_ms,
            until_ms,
            filter,
            errors_only,
            None,
        );
        let capped = records.len() > SUMMARY_SCAN_CAP;
        let mut out = RequestsSummary {
            capped,
            ..Default::default()
        };
        // Latency percentiles need the values, not a running sum. Bounded
        // by the scan cap above, so this allocation is bounded too.
        let mut latencies: Vec<u64> = Vec::with_capacity(records.len().min(SUMMARY_SCAN_CAP));
        for rec in records.iter().take(SUMMARY_SCAN_CAP) {
            out.requests += 1;
            if rec.status >= 400 {
                out.errors += 1;
            }
            out.attempts += u64::from(rec.attempts);
            out.input_tokens += rec.input_tokens;
            out.output_tokens += rec.output_tokens;
            out.cached_tokens += rec.cached_tokens;
            out.cost_micro_usd += rec.cost_micro_usd;
            latencies.push(rec.latency_ms);
        }
        latencies.sort_unstable();
        out.p50_latency_ms = percentile(&latencies, 50);
        out.p95_latency_ms = percentile(&latencies, 95);
        out
    }
}

/// Nearest-rank percentile over a sorted slice. Zero for an empty set,
/// which reads as "no data" rather than a confident zero-millisecond p95.
fn percentile(sorted: &[u64], p: u64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p as f64 / 100.0 * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

/// Everything the Usage page needs for a window, from one scan.
///
/// The page used to derive all of this client-side from a single page of
/// 1,000 records, so every figure on it was really "of the most recent
/// thousand". On a gateway doing six figures a day that made "Last 24
/// hours" and "Last hour" report the same number, and neither was the
/// answer. Ranges over a day went to `/history` and were correct, which is
/// why only those looked right.
///
/// Grouped and bucketed here rather than in three more round trips: the
/// scan is the expensive part, and it is the same scan for all of them.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct UsageSummary {
    pub requests: u64,
    pub errors: u64,
    pub attempts: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_tokens: u64,
    pub cost_micro_usd: u64,
    pub p50_latency_ms: u64,
    pub p95_latency_ms: u64,
    /// The scan hit its ceiling, so these are a floor. Reported, not hidden.
    pub capped: bool,
    pub by_model: Vec<UsageSlice>,
    pub by_provider: Vec<UsageSlice>,
    pub by_key: Vec<UsageSlice>,
    /// Bucket start (unix seconds) -> totals, for the trend charts.
    pub series: Vec<UsageBucket>,
    /// The busiest few models' latency over the same buckets, so "which
    /// model got slower" is one chart rather than a filter applied seven
    /// times. Capped at [`LATENCY_SERIES_CAP`] series.
    pub latency_by_model: Vec<ModelLatency>,
    /// Bucket width actually used, so the chart can label itself.
    pub bucket_secs: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct UsageSlice {
    pub name: String,
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    /// Summed; the mean is this over `requests`.
    pub latency_ms_sum: u64,
    /// Read from this slice's own histogram, which is why it is here and
    /// not derived from the sum: a mean hides the tail that makes one
    /// model feel slow.
    pub p95_latency_ms: u64,
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct UsageBucket {
    pub ts: u64,
    pub requests: u64,
    pub failed: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cost_micro_usd: u64,
    pub latency_ms_sum: u64,
}

/// Slices under construction: the numbers that sum, alongside the
/// distribution that does not, kept apart because only the first is sent.
type Slices = HashMap<String, (UsageSlice, LatencyHistogram)>;

fn slot<'a>(map: &'a mut Slices, name: &str) -> &'a mut (UsageSlice, LatencyHistogram) {
    map.entry(name.to_owned()).or_insert_with(|| {
        (
            UsageSlice {
                name: name.to_owned(),
                ..Default::default()
            },
            LatencyHistogram::default(),
        )
    })
}

/// Finish the slices: read each one's p95 off its histogram, then order
/// them biggest first — the tables show a leaderboard, not an index.
fn ranked(map: Slices) -> Vec<UsageSlice> {
    let mut out: Vec<UsageSlice> = map
        .into_values()
        .map(|(mut slice, latency)| {
            slice.p95_latency_ms = if latency.is_empty() {
                slice
                    .latency_ms_sum
                    .checked_div(slice.requests)
                    .unwrap_or(0)
            } else {
                latency.percentile(95)
            };
            slice
        })
        .collect();
    out.sort_by(|a, b| {
        b.requests
            .cmp(&a.requests)
            .then_with(|| a.name.cmp(&b.name))
    });
    out
}

/// Keep the chart under a few hundred points however wide the window is:
/// a minute bucket over 24h is 1,440 points, which is more than the pixels
/// available and more than uPlot should be asked to draw.
fn bucket_width_secs(span_ms: u64) -> u64 {
    const TARGET_POINTS: u64 = 240;
    let span_secs = (span_ms / 1000).max(1);
    let raw = span_secs.div_ceil(TARGET_POINTS).max(60);
    // Round up to a friendly step so ticks land on readable times.
    for step in [60, 300, 900, 1800, 3600, 7200, 21600, 86400] {
        if raw <= step {
            return step;
        }
    }
    86400
}

impl UsagePipeline {
    /// Aggregate a window into totals, groupings and a trend series.
    ///
    /// Served from rollups whenever they can answer, which is every
    /// default view the console opens with. The raw-record path below is
    /// kept for the two cases rollups genuinely cannot cover: an
    /// errors-only view (a rollup counts failures but does not carry
    /// their tokens or their latency separately) and a gateway with no
    /// data directory, where the in-memory ring is the entire history.
    pub fn usage_summary(
        &self,
        since_ms: u64,
        until_ms: u64,
        filter: &HistoryFilter,
        errors_only: bool,
    ) -> UsageSummary {
        let bucket_secs = bucket_width_secs(until_ms.saturating_sub(since_ms));
        // Same reasoning as `summary`: rollups have no caller
        // dimensions, so a filter naming one falls back to records.
        if !errors_only && self.data_dir.is_some() && !filter.needs_records() {
            return self.usage_summary_from_rollups(since_ms, until_ms, filter, bucket_secs);
        }
        let records = self.collect_page(
            SUMMARY_SCAN_CAP + 1,
            since_ms,
            until_ms,
            filter,
            errors_only,
            None,
        );
        // Two different ceilings, both of which make these figures a floor.
        // The scan cap is the obvious one. The other is subtler: with no
        // usage directory configured nothing is ever flushed, so the
        // in-memory ring is the entire history and a full ring means older
        // traffic is simply gone. Reporting that as an exact total is the
        // same lie this endpoint was written to remove.
        let ring_is_everything = self.data_dir.is_none() && records.len() >= RECENT_CAP;
        let mut out = UsageSummary {
            capped: records.len() > SUMMARY_SCAN_CAP || ring_is_everything,
            bucket_secs,
            ..Default::default()
        };

        let mut latencies: Vec<u64> = Vec::with_capacity(records.len().min(SUMMARY_SCAN_CAP));
        let mut by_model = Slices::new();
        let mut by_provider = Slices::new();
        let mut by_key = Slices::new();
        let mut series: BTreeMap<u64, UsageBucket> = BTreeMap::new();
        let mut grid = LatencyGrid::default();

        for rec in records.iter().take(SUMMARY_SCAN_CAP) {
            out.requests += 1;
            let failed = u64::from(rec.status >= 400);
            out.errors += failed;
            out.attempts += u64::from(rec.attempts);
            out.input_tokens += rec.input_tokens;
            out.output_tokens += rec.output_tokens;
            out.cached_tokens += rec.cached_tokens;
            out.cost_micro_usd += rec.cost_micro_usd;
            latencies.push(rec.latency_ms);

            let add = |map: &mut Slices, name: &str| {
                let (slice, latency) = slot(map, name);
                slice.requests += 1;
                slice.failed += failed;
                slice.input_tokens += rec.input_tokens;
                slice.output_tokens += rec.output_tokens;
                slice.cost_micro_usd += rec.cost_micro_usd;
                slice.latency_ms_sum += rec.latency_ms;
                latency.record(rec.latency_ms);
            };
            let model = model_name(&rec.model);
            add(&mut by_model, model);
            add(&mut by_provider, model_name(&rec.provider));
            add(&mut by_key, rec.vkey.as_deref().unwrap_or("(none)"));

            let bucket = (rec.ts / 1000 / bucket_secs) * bucket_secs;
            let point = series.entry(bucket).or_insert(UsageBucket {
                ts: bucket,
                ..Default::default()
            });
            point.requests += 1;
            point.failed += failed;
            point.input_tokens += rec.input_tokens;
            point.output_tokens += rec.output_tokens;
            point.cost_micro_usd += rec.cost_micro_usd;
            point.latency_ms_sum += rec.latency_ms;
            grid.add(model, bucket, 1, rec.latency_ms);
        }

        latencies.sort_unstable();
        out.p50_latency_ms = percentile(&latencies, 50);
        out.p95_latency_ms = percentile(&latencies, 95);

        out.by_model = ranked(by_model);
        out.by_provider = ranked(by_provider);
        out.by_key = ranked(by_key);
        out.series = series.into_values().collect();
        out.latency_by_model = grid.top(LATENCY_SERIES_CAP);
        out
    }

    /// The same summary, from rollup rows instead of raw records.
    ///
    /// Never capped: it reads the whole window rather than the first two
    /// hundred thousand records in it, so the figures are the window's
    /// and not a floor. The rows it reads are bounded by the window's
    /// resolution, which is why that is affordable at all.
    fn usage_summary_from_rollups(
        &self,
        since_ms: u64,
        until_ms: u64,
        filter: &HistoryFilter,
        bucket_secs: u64,
    ) -> UsageSummary {
        // Hourly rows can be re-bucketed into anything an hour or wider;
        // below that the window is short enough that a day's rows are
        // already in memory, so hourly costs nothing either way.
        let hourly = bucket_secs < 86_400;
        let rows = self.rollups_for_window(since_ms, until_ms, hourly);

        // Sub-hour buckets are the one thing rollups cannot draw — an
        // hourly row is one point, and asking for fifteen-minute
        // resolution from it yields a chart with a quarter of its points
        // and three-quarters of them zero. The minute aggregate is the
        // source with that resolution, and it spans exactly the day
        // these windows fall inside.
        //
        // When it cannot cover the window — a process that restarted
        // inside it — the series widens to hourly and says so through
        // `bucket_secs`, so the chart labels itself for what it actually
        // drew instead of implying detail it does not have.
        let now = vkey::unix_now_ms();
        let (minute_series, minute_grid) = match (bucket_secs < HOUR_MS / 1000)
            .then(|| self.agg.floor_minute(now))
            .flatten()
            .filter(|floor| since_ms / 60_000 >= *floor)
        {
            Some(_) => {
                let (series, grid) = self.agg.series(since_ms, until_ms, bucket_secs, filter);
                (Some(series), Some(grid))
            }
            None => (None, None),
        };

        let mut out = UsageSummary {
            bucket_secs: match &minute_series {
                Some(_) => bucket_secs,
                None => bucket_secs.max(HOUR_MS / 1000),
            },
            ..Default::default()
        };
        let mut latency = LatencyHistogram::default();
        let mut latency_sum = 0u64;
        let mut by_model = Slices::new();
        let mut by_provider = Slices::new();
        let mut by_key = Slices::new();
        let mut series: BTreeMap<u64, UsageBucket> = BTreeMap::new();
        // Hourly rows when the minute aggregate is not drawing the
        // series, and the aggregate's own per-model split when it is —
        // the same choice the series itself makes, for the same reason:
        // counting both would count the traffic twice.
        let mut grid = minute_grid.unwrap_or_default();

        for row in &rows {
            if !filter.matches_dims(&row.provider, &row.model, row.vkey.as_deref()) {
                continue;
            }
            out.requests += row.requests;
            out.errors += row.failed;
            out.attempts += row.attempts;
            out.input_tokens += row.input_tokens;
            out.output_tokens += row.output_tokens;
            out.cached_tokens += row.cached_tokens;
            out.cost_micro_usd += row.cost_micro_usd;
            latency.merge(&row.latency);
            latency_sum += row.latency_ms_sum;

            let add = |map: &mut Slices, name: &str| {
                let (slice, latency) = slot(map, name);
                slice.requests += row.requests;
                slice.failed += row.failed;
                slice.input_tokens += row.input_tokens;
                slice.output_tokens += row.output_tokens;
                slice.cost_micro_usd += row.cost_micro_usd;
                slice.latency_ms_sum += row.latency_ms_sum;
                latency.merge(&row.latency);
            };
            let model = model_name(&row.model);
            add(&mut by_model, model);
            add(&mut by_provider, model_name(&row.provider));
            add(&mut by_key, row.vkey.as_deref().unwrap_or("(none)"));

            // Skipped when the minute aggregate is drawing the series:
            // the two would be the same traffic counted twice.
            if minute_series.is_some() {
                continue;
            }
            let bucket = (row.hour_ms / 1000 / out.bucket_secs) * out.bucket_secs;
            let point = series.entry(bucket).or_insert(UsageBucket {
                ts: bucket,
                ..Default::default()
            });
            point.requests += row.requests;
            point.failed += row.failed;
            point.input_tokens += row.input_tokens;
            point.output_tokens += row.output_tokens;
            point.cost_micro_usd += row.cost_micro_usd;
            point.latency_ms_sum += row.latency_ms_sum;
            grid.add(model, bucket, row.requests, row.latency_ms_sum);
        }

        // Rows written before histograms existed carry only a sum. The
        // mean is the only percentile a sum can honestly supply, and
        // saying "p50 = mean" is better than saying "p50 = 0" for a
        // window whose older half predates the upgrade.
        out.p50_latency_ms = if latency.is_empty() {
            latency_sum.checked_div(out.requests).unwrap_or(0)
        } else {
            latency.percentile(50)
        };
        out.p95_latency_ms = if latency.is_empty() {
            latency_sum.checked_div(out.requests).unwrap_or(0)
        } else {
            latency.percentile(95)
        };

        out.by_model = ranked(by_model);
        out.by_provider = ranked(by_provider);
        out.by_key = ranked(by_key);
        out.series = minute_series.unwrap_or_else(|| series.into_values().collect());
        out.latency_by_model = grid.top(LATENCY_SERIES_CAP);
        out
    }
}

/// A request's bodies, stored apart from its metadata.
///
/// Its own stream, keyed by request id: a log listing reads metadata for
/// hundreds of requests and needs none of this, and bodies are two
/// orders of magnitude larger. Mixing them would make every listing pay
/// for a payload it does not read.
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
/// The per-day map from request id to the file holding its bodies.
///
/// Appended as each batch is written, so it costs one line per captured
/// request and turns "open one request" from a walk of the whole day's
/// partition into a single file read. Plain text rather than compressed:
/// it is read far more often than it is written, and a lookup that has
/// to decompress an index has not saved very much.
const BODIES_INDEX: &str = "index.tsv";

/// Which file in a day partition holds a request's bodies.
fn bodies_index_lookup(day_dir: &std::path::Path, request_id: &str) -> Option<String> {
    let text = std::fs::read_to_string(day_dir.join(BODIES_INDEX)).ok()?;
    // Last match wins: a body written twice for one id (a retry that
    // reused it) should resolve to the most recent.
    text.lines()
        .rev()
        .find_map(|line| line.strip_prefix(request_id)?.strip_prefix('\t'))
        .map(str::to_owned)
}

/// One body record out of one file, or `None` if it is not in there.
fn scan_bodies_file(path: &std::path::Path, request_id: &str) -> Option<RequestBodies> {
    let handle = std::fs::File::open(path).ok()?;
    let decoder = zstd::Decoder::new(handle).ok()?;
    std::io::BufRead::lines(std::io::BufReader::new(decoder))
        .map_while(Result::ok)
        // A cheap reject before paying for a full parse: bodies are
        // large, and most lines in a file are not the one being sought.
        .filter(|line| line.contains(request_id))
        .filter_map(|line| serde_json::from_str::<RequestBodies>(&line).ok())
        .find(|bodies| bodies.request_id == request_id)
}

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
        let name = format!("{node}-{seq:08}.jsonl.zst");
        // The index is appended before the data. A crash between the two
        // leaves an index entry pointing at a file that does not exist,
        // which reads as "not found" and falls through to the scan — the
        // safe failure. The other order would leave bodies that no
        // lookup could find without one.
        if let Ok(mut index) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(day_dir.join(BODIES_INDEX))
        {
            for body in &bodies {
                let _ = writeln!(index, "{}\t{name}", body.request_id);
            }
        }
        let path = day_dir.join(&name);
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
///
/// What this must not do is count traffic twice, and the only rows at
/// risk are the ones `history` has already read off local disk. Those
/// are identified by the file that holds them, not by the node that
/// wrote it: node identity is minted fresh every boot, so a gateway that
/// keeps its data dir across a restart would no longer recognise its own
/// shipped objects and would add them to the rows it just read locally.
pub async fn fleet_rollups(
    store: &router_store::Store,
    days: u32,
    local_dir: &std::path::Path,
) -> Vec<RollupRow> {
    let cutoff = day_partition(vkey::unix_now_ms().saturating_sub(days.max(1) as u64 * 86_400_000));
    let local = local_rollup_files(&local_dir.join("rollup"));
    let Ok(keys) = store.list_blobs("rollup/").await else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for key in keys {
        // rollup/dt=YYYY-MM-DD/node=<id>/<file>
        let mut parts = key.split('/');
        let (Some(_), Some(day), Some(_), Some(file)) =
            (parts.next(), parts.next(), parts.next(), parts.next())
        else {
            continue;
        };
        if day < cutoff.as_str() {
            continue;
        }
        // Already read from local disk, whichever identity shipped it.
        if local.contains(&(day.to_owned(), file.to_owned())) {
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

/// Every rollup file on local disk, as the `(day, file name)` pair that
/// also names its shipped object. Rollup file names carry the writing
/// node's id, so a name is unique across the fleet without the node
/// segment — which is what lets this survive the id changing.
fn local_rollup_files(rollup_dir: &std::path::Path) -> BTreeSet<(String, String)> {
    let mut out = BTreeSet::new();
    let Ok(days) = std::fs::read_dir(rollup_dir) else {
        return out;
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
            let name = file.file_name().to_string_lossy().into_owned();
            if name.ends_with(".zst") {
                out.insert((day_name.clone(), name));
            }
        }
    }
    out
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
    let rollup_days = retention_days.saturating_mul(ROLLUP_RETENTION_FACTOR);
    prune_partitions(&dir.join("rollup"), rollup_days, "rollup");
    // The read caches are derived from those partitions, so they go when
    // their sources do — a cache for a day that no longer exists can
    // never be validated again, and would sit there forever.
    crate::rollup_cache::prune(
        dir,
        &day_partition(vkey::unix_now_ms().saturating_sub(rollup_days as u64 * DAY_MS)),
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

/// Rollup rows with no records behind them, for exercising volumes it
/// would be silly to materialise a record apiece for.
#[doc(hidden)]
#[cfg(test)]
pub(crate) fn write_rollup_rows_for_test(
    dir: &std::path::Path,
    node: &str,
    seq: u64,
    rows: &[RollupRow],
) {
    write_rollups(dir, node, seq, rows).expect("test corpus writes");
}

#[cfg(test)]
mod usage_summary_tests {
    use super::*;

    fn record(i: u64, model: &str, status: u16) -> UsageRecord {
        UsageRecord {
            ts: 1_700_000_000_000 + i * 1000,
            request_id: format!("req-{i}"),
            endpoint: "chat".into(),
            requested: model.into(),
            provider: "mock".into(),
            model: model.into(),
            vkey: Some("vk-1".into()),
            status,
            stream: false,
            input_tokens: 10,
            output_tokens: 5,
            cached_tokens: 0,
            cost_micro_usd: 100,
            latency_ms: i % 50,
            overhead_us: 10,
            attempts: 1,
            tag: None,
            prompt: None,
            meta: BTreeMap::new(),
            error_class: None,
            seat: None,
            ttft_ms: None,
            queue_lag_ms: None,
        }
    }

    /// The bug this endpoint exists to fix: the Usage page derived every
    /// figure from one page of 1,000 records, so a window holding more
    /// than that reported 1,000 and called it the total.
    ///
    /// Written against a flushed store rather than the bare ring, because
    /// that is the shape production has — the ring holds `RECENT_CAP` and
    /// everything older is read back from the partitions.
    #[test]
    fn counts_past_the_page_size_that_used_to_cap_the_page() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let records: Vec<UsageRecord> = (0..2_500).map(|i| record(i, "gpt-4o", 200)).collect();
        // Flushed in batches, the way the writer actually does it.
        for (batch, chunk) in records.chunks(500).enumerate() {
            write_batch_for_test(dir.path(), "node-a", batch as u64, chunk);
        }
        let summary = pipeline.usage_summary(0, u64::MAX, &HistoryFilter::default(), false);
        assert_eq!(
            summary.requests, 2_500,
            "every request in the window, not the first thousand"
        );
        assert_eq!(summary.input_tokens, 25_000);
        assert_eq!(summary.output_tokens, 12_500);
        assert!(!summary.capped);
    }

    /// The rollup path and the raw-record path must not disagree.
    ///
    /// They are read from the same batches by construction, so this is
    /// less a test of arithmetic than of the plumbing between them:
    /// which tier answered, whether the window filter admitted the same
    /// hours, whether a dimension got lost on the way through a rollup.
    #[test]
    fn rollups_and_raw_records_report_the_same_window() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let records: Vec<UsageRecord> = (0..3_000)
            .map(|i| {
                let mut rec = record(i, if i % 3 == 0 { "gpt-4o" } else { "o3" }, 200);
                if i % 7 == 0 {
                    rec.status = 500;
                }
                rec
            })
            .collect();
        for (batch, chunk) in records.chunks(400).enumerate() {
            write_batch_for_test(dir.path(), "node-a", batch as u64, chunk);
            write_rollups_for_test(dir.path(), "node-a", batch as u64, chunk);
        }
        let since = records.first().expect("records").ts;
        let until = records.last().expect("records").ts;

        let rollup = pipeline.usage_summary(since, until, &HistoryFilter::default(), false);
        // `errors_only` is the one view rollups cannot serve, so it is
        // also the way to reach the raw path for a comparison.
        let raw = {
            let mut summary = UsageSummary::default();
            for rec in &records {
                summary.requests += 1;
                summary.errors += u64::from(rec.status >= 400);
                summary.input_tokens += rec.input_tokens;
                summary.output_tokens += rec.output_tokens;
                summary.cost_micro_usd += rec.cost_micro_usd;
                summary.attempts += u64::from(rec.attempts);
            }
            summary
        };

        assert_eq!(rollup.requests, raw.requests);
        assert_eq!(rollup.errors, raw.errors);
        assert_eq!(rollup.attempts, raw.attempts);
        assert_eq!(rollup.input_tokens, raw.input_tokens);
        assert_eq!(rollup.output_tokens, raw.output_tokens);
        assert_eq!(rollup.cost_micro_usd, raw.cost_micro_usd);
        assert!(
            !rollup.capped,
            "a rollup read covers the window, never a floor"
        );
        assert_eq!(rollup.by_model.len(), 2);
    }

    /// Latency has to reach the console split by model, because "the
    /// gateway got slower" and "one model got slower" are different
    /// incidents and only the second one is actionable.
    ///
    /// Both paths are checked against the same corpus: a mean that
    /// survives a rollup and a mean computed from records have to agree,
    /// or the chart changes shape when the window crosses a day.
    #[test]
    fn latency_reaches_the_summary_split_by_model() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        // Two models with deliberately different latencies, so a chart
        // that averaged them together would show neither.
        let records: Vec<UsageRecord> = (0..600)
            .map(|i| {
                let slow = i % 2 == 0;
                let mut rec = record(i, if slow { "slow-model" } else { "fast-model" }, 200);
                rec.latency_ms = if slow { 1_000 } else { 100 };
                rec
            })
            .collect();
        for (batch, chunk) in records.chunks(200).enumerate() {
            write_batch_for_test(dir.path(), "node-a", batch as u64, chunk);
            write_rollups_for_test(dir.path(), "node-a", batch as u64, chunk);
        }
        let since = records.first().expect("records").ts;
        let until = records.last().expect("records").ts;

        let summary = pipeline.usage_summary(since, until, &HistoryFilter::default(), false);
        let mean = |slice: &UsageSlice| slice.latency_ms_sum / slice.requests;
        let by_name = |name: &str| {
            summary
                .by_model
                .iter()
                .find(|slice| slice.name == name)
                .unwrap_or_else(|| panic!("{name} missing from by_model"))
        };
        assert_eq!(mean(by_name("slow-model")), 1_000);
        assert_eq!(mean(by_name("fast-model")), 100);
        // Every request in a slice took the same time, so its p95 is
        // that time — modulo the histogram's ~19% bucket width.
        assert!(
            (1_000..=1_200).contains(&by_name("slow-model").p95_latency_ms),
            "p95 came back as {}",
            by_name("slow-model").p95_latency_ms,
        );

        let series = &summary.latency_by_model;
        assert_eq!(series.len(), 2, "one series per model served");
        for model in series {
            let requests: u64 = model.points.iter().map(|p| p.requests).sum();
            let latency: u64 = model.points.iter().map(|p| p.latency_ms_sum).sum();
            assert_eq!(requests, 300, "{} lost requests", model.name);
            assert_eq!(
                latency / requests,
                if model.name == "slow-model" {
                    1_000
                } else {
                    100
                },
                "{} charted the wrong mean",
                model.name,
            );
        }

        // The buckets also have to total the window: a series drawn from
        // a different denominator than the tiles above it is worse than
        // no series at all.
        let charted: u64 = series
            .iter()
            .flat_map(|m| &m.points)
            .map(|p| p.requests)
            .sum();
        assert_eq!(charted, summary.requests);
    }

    /// A window wider than the live tail is read as day buckets, and
    /// those have to carry latency too — otherwise the per-model chart
    /// goes blank the moment somebody picks "Last 7 days".
    #[test]
    fn history_carries_latency_per_day_and_a_window_percentile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let now = vkey::unix_now_ms();
        let records: Vec<UsageRecord> = (0..400)
            .map(|i| {
                let mut rec = record(i, "gpt-4o", 200);
                rec.ts = now - (i % 4) * DAY_MS - HOUR_MS;
                rec.latency_ms = 250;
                rec
            })
            .collect();
        write_rollups_for_test(dir.path(), "node-a", 0, &records);

        let history = pipeline.history_all(30, &HistoryFilter::default());
        let days = &history.data["model"]["gpt-4o"];
        assert_eq!(days.len(), 4, "one bucket per day served");
        for day in days {
            assert_eq!(
                day.latency_ms_sum / day.requests,
                250,
                "{} lost its latency",
                day.day
            );
        }
        assert!(
            (250..=300).contains(&history.p95_latency_ms),
            "window p95 came back as {}",
            history.p95_latency_ms,
        );
    }

    /// The scan cap was the thing that made a wide window a lie. Past it,
    /// the old path silently stopped counting; the rollup path has no
    /// such ceiling because it never touches a record.
    #[test]
    fn a_window_past_the_scan_cap_is_still_exact() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        // Rollup rows only — the point is that the record count is
        // irrelevant to what this costs and what it reports.
        let mut rows = Vec::new();
        for hour in 0..24u64 {
            rows.push(RollupRow {
                requests: 500_000,
                ..RollupRow::empty(
                    (1_700_000_000_000 / HOUR_MS + hour) * HOUR_MS,
                    "mock",
                    "gpt-4o",
                    None,
                )
            });
        }
        write_rollup_rows_for_test(dir.path(), "node-a", 0, &rows);

        let start = rows.first().expect("rows").hour_ms;
        let end = rows.last().expect("rows").hour_ms + HOUR_MS;
        let summary = pipeline.usage_summary(start, end, &HistoryFilter::default(), false);
        assert_eq!(summary.requests, 12_000_000);
        assert!(!summary.capped);
    }

    /// Percentiles have to survive the trip through a rollup, or the
    /// Logs header silently reports zero for every window wider than the
    /// live tail.
    #[test]
    fn latency_percentiles_survive_the_rollup() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let records: Vec<UsageRecord> = (1..=1_000)
            .map(|i| {
                let mut rec = record(i, "gpt-4o", 200);
                rec.latency_ms = i;
                rec
            })
            .collect();
        write_rollups_for_test(dir.path(), "node-a", 0, &records);

        let since = records.first().expect("records").ts;
        let until = records.last().expect("records").ts;
        let summary = pipeline.summary(since, until, &HistoryFilter::default(), false);
        // True p50 is 500 and p95 is 950; buckets report the upper bound,
        // so never under and never more than a bucket over.
        assert!((500..=600).contains(&summary.p50_latency_ms), "{summary:?}");
        assert!(
            (950..=1140).contains(&summary.p95_latency_ms),
            "{summary:?}"
        );
    }

    /// Every grouping from one read has to equal the groupings from
    /// three, or the console shows different numbers than the API does.
    #[test]
    fn one_history_read_matches_the_three_it_replaced() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let now = vkey::unix_now_ms();
        let records: Vec<UsageRecord> = (0..500)
            .map(|i| {
                let mut rec = record(i, if i % 2 == 0 { "gpt-4o" } else { "o3" }, 200);
                rec.ts = now - (i % 5) * DAY_MS - HOUR_MS;
                rec
            })
            .collect();
        write_rollups_for_test(dir.path(), "node-a", 0, &records);

        let all = pipeline.history_all(30, &HistoryFilter::default());
        for by in ["", "provider", "model", "key"] {
            let one = pipeline.history(30, by, &HistoryFilter::default());
            let from_all = all.data.get(by).cloned().unwrap_or_default();
            assert_eq!(one, from_all, "grouping {by:?} disagreed");
        }
        let total: u64 = all.data["model"]
            .values()
            .flatten()
            .map(|bucket| bucket.requests)
            .sum();
        assert_eq!(total, 500);
    }

    /// A short window has to keep its short buckets.
    ///
    /// Rollups are hourly, so serving the "Last hour" chart from them
    /// alone would draw a single point and call it a trend. The minute
    /// aggregate supplies the resolution; the rollups still supply the
    /// totals.
    #[test]
    fn a_sub_hour_window_charts_at_sub_hour_resolution() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let now = vkey::unix_now_ms();
        // An hour of traffic, one request a minute.
        let records: Vec<UsageRecord> = (0..=60)
            .map(|i| {
                let mut rec = record(i, "gpt-4o", 200);
                rec.ts = now - (60 - i) * 60_000;
                rec
            })
            .collect();
        for rec in &records {
            pipeline.record(rec.clone());
        }
        write_rollups_for_test(dir.path(), "node-a", 0, &records);

        let summary =
            pipeline.usage_summary(now - 3_600_000, now, &HistoryFilter::default(), false);
        assert_eq!(summary.requests, 61, "totals still come from the rollups");
        assert!(
            summary.bucket_secs < 3600,
            "an hour-wide window must not be charted in one-hour buckets",
        );
        assert!(
            summary.series.len() > 1,
            "a trend needs more than one point: {:?}",
            summary.series,
        );
        let charted: u64 = summary.series.iter().map(|b| b.requests).sum();
        assert_eq!(charted, 61, "the series and the totals must agree");
    }

    /// The aggregate is per process, so a window reaching back before
    /// this one started cannot be charted at minute resolution. Widening
    /// the buckets is the honest answer; drawing zeros is not.
    #[test]
    fn a_window_older_than_the_process_widens_its_buckets() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let now = vkey::unix_now_ms();
        let records: Vec<UsageRecord> = (0..60)
            .map(|i| {
                let mut rec = record(i, "gpt-4o", 200);
                rec.ts = now - (59 - i) * 60_000;
                rec
            })
            .collect();
        // Flushed, but never recorded through this process's aggregate —
        // which is what a restart looks like.
        write_rollups_for_test(dir.path(), "node-a", 0, &records);

        let summary =
            pipeline.usage_summary(now - 3_600_000, now, &HistoryFilter::default(), false);
        assert_eq!(summary.requests, 60);
        assert_eq!(
            summary.bucket_secs, 3600,
            "buckets widen to what is readable"
        );
        let charted: u64 = summary.series.iter().map(|b| b.requests).sum();
        assert_eq!(charted, 60, "widening must not lose traffic");
    }

    /// The other ceiling: with nothing flushed, the ring is the whole
    /// history, so a full ring has to say it is a floor.
    #[test]
    fn a_full_ring_with_no_store_reports_itself_as_a_floor() {
        let pipeline = UsagePipeline::for_test(None);
        for i in 0..1_500 {
            pipeline.record(record(i, "gpt-4o", 200));
        }
        let summary = pipeline.usage_summary(0, u64::MAX, &HistoryFilter::default(), false);
        assert_eq!(summary.requests, RECENT_CAP as u64);
        assert!(
            summary.capped,
            "1,000 of 1,500 must not be presented as the total"
        );
    }

    #[test]
    fn groups_are_ranked_and_name_the_unrouted() {
        let pipeline = UsagePipeline::for_test(None);
        for i in 0..10 {
            pipeline.record(record(i, "gpt-4o", 200));
        }
        for i in 10..13 {
            pipeline.record(record(i, "claude", 200));
        }
        // A request that never reached a provider carries no model; it is
        // still a request and must not vanish into an empty group name.
        let mut orphan = record(99, "", 404);
        orphan.provider = String::new();
        pipeline.record(orphan);

        let summary = pipeline.usage_summary(0, u64::MAX, &HistoryFilter::default(), false);
        assert_eq!(summary.requests, 14);
        assert_eq!(summary.errors, 1);
        let names: Vec<(&str, u64)> = summary
            .by_model
            .iter()
            .map(|s| (s.name.as_str(), s.requests))
            .collect();
        assert_eq!(names, vec![("gpt-4o", 10), ("claude", 3), ("(none)", 1)]);
    }

    #[test]
    fn totals_agree_with_the_sum_of_every_grouping() {
        let pipeline = UsagePipeline::for_test(None);
        for i in 0..40 {
            pipeline.record(record(i, if i % 2 == 0 { "a" } else { "b" }, 200));
        }
        let s = pipeline.usage_summary(0, u64::MAX, &HistoryFilter::default(), false);
        for group in [&s.by_model, &s.by_provider, &s.by_key] {
            assert_eq!(
                group.iter().map(|g| g.requests).sum::<u64>(),
                s.requests,
                "a grouping that does not re-sum to the total is a grouping that lost rows"
            );
        }
        assert_eq!(
            s.series.iter().map(|b| b.requests).sum::<u64>(),
            s.requests,
            "the chart and the header must describe the same traffic"
        );
    }

    #[test]
    fn buckets_stay_readable_however_wide_the_window() {
        // A minute bucket over 24h is 1,440 points — more than the pixels
        // available. Each step is a round number so ticks land sensibly.
        assert_eq!(bucket_width_secs(60 * 60 * 1000), 60);
        assert_eq!(bucket_width_secs(24 * 60 * 60 * 1000), 900);
        assert!(bucket_width_secs(30 * 24 * 60 * 60 * 1000) >= 7200);
        assert_eq!(bucket_width_secs(0), 60, "an empty span still has a width");
    }

    #[test]
    fn a_filter_narrows_the_totals_and_the_groups_together() {
        let pipeline = UsagePipeline::for_test(None);
        for i in 0..20 {
            pipeline.record(record(i, if i < 5 { "a" } else { "b" }, 200));
        }
        let filter = HistoryFilter {
            model: Some("a".into()),
            ..Default::default()
        };
        let s = pipeline.usage_summary(0, u64::MAX, &filter, false);
        assert_eq!(s.requests, 5);
        assert_eq!(s.by_model.len(), 1);
        assert_eq!(s.by_model[0].requests, 5);
    }
}

#[cfg(test)]
mod prompt_tests {
    use super::{RequestsSummary, percentile, prompt_preview};
    use serde_json::json;

    #[test]
    fn chat_completions_prefers_the_first_user_turn() {
        let body = json!({
            "messages": [
                {"role": "system", "content": "Be terse."},
                {"role": "user", "content": "what is the capital of France?"},
                {"role": "user", "content": "and of Spain?"},
            ]
        })
        .to_string();
        assert_eq!(
            prompt_preview(&body).as_deref(),
            Some("what is the capital of France?")
        );
    }

    #[test]
    fn anthropic_system_is_outside_messages() {
        // No user turn at all, so the system prompt is the only thing to
        // show — and Anthropic keeps it in its own field.
        let body = json!({"system": "You are a linter.", "messages": []}).to_string();
        assert_eq!(prompt_preview(&body).as_deref(), Some("You are a linter."));
    }

    #[test]
    fn responses_instructions_are_the_fallback() {
        let body = json!({"instructions": "You are Codex.", "input": []}).to_string();
        assert_eq!(prompt_preview(&body).as_deref(), Some("You are Codex."));
    }

    #[test]
    fn attachments_are_named_rather_than_dropped() {
        // A prompt that is only an attachment would otherwise read as
        // empty, which is exactly the request worth spotting in a log.
        let body = json!({
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "what is wrong here?"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAA"}},
                {"type": "file", "file": {"filename": "chart.pdf"}},
            ]}]
        })
        .to_string();
        assert_eq!(
            prompt_preview(&body).as_deref(),
            Some("what is wrong here? [image] [document]")
        );
    }

    #[test]
    fn responses_skips_non_message_items() {
        // A Responses `input` array carries function_call items with no
        // role; walking them as turns would read `arguments` as a prompt.
        let body = json!({"input": [
            {"type": "function_call", "name": "shell", "arguments": "{}"},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hello"}]},
        ]})
        .to_string();
        assert_eq!(prompt_preview(&body).as_deref(), Some("hello"));
    }

    #[test]
    fn newlines_collapse_so_a_row_stays_one_line() {
        let body = json!({"messages": [{"role": "user", "content": "a\n\n   b\tc"}]}).to_string();
        assert_eq!(prompt_preview(&body).as_deref(), Some("a b c"));
    }

    #[test]
    fn truncation_lands_on_a_character_not_a_byte() {
        let long = "\u{1f600}".repeat(400);
        let body = json!({"messages": [{"role": "user", "content": long}]}).to_string();
        let preview = prompt_preview(&body).expect("a preview");
        assert_eq!(preview.chars().count(), 241, "240 chars plus the ellipsis");
        assert!(preview.ends_with('\u{2026}'));
    }

    #[test]
    fn a_body_that_is_not_json_yields_nothing() {
        assert!(prompt_preview("not json at all").is_none());
        assert!(prompt_preview("{}").is_none());
    }

    #[test]
    fn an_empty_prompt_is_none_not_an_empty_string() {
        let body = json!({"messages": [{"role": "user", "content": "   "}]}).to_string();
        assert!(prompt_preview(&body).is_none());
    }

    #[test]
    fn percentiles_are_nearest_rank_and_safe_when_empty() {
        let values: Vec<u64> = (1..=100).collect();
        assert_eq!(percentile(&values, 50), 50);
        assert_eq!(percentile(&values, 95), 95);
        assert_eq!(percentile(&[], 95), 0, "no data is 0, not a panic");
        assert_eq!(percentile(&[7], 95), 7);
    }

    #[test]
    fn a_default_summary_is_all_zero() {
        let s = RequestsSummary::default();
        assert_eq!(s.requests, 0);
        assert!(!s.capped);
    }
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
            prompt: None,
            meta: BTreeMap::new(),
            error_class: None,
            seat: None,
            ttft_ms: None,
            queue_lag_ms: None,
        }
    }

    /// Opening a request must find its bodies by whichever route is
    /// available, and the three must agree about what they found.
    ///
    /// The scan is what every lookup used to do — open every file in the
    /// day partition and decompress it looking for a substring. It is
    /// kept because partitions written before the index exists still
    /// need answering, and it is tested because it is now the path least
    /// likely to be exercised in anger.
    #[test]
    fn a_request_body_is_found_by_index_and_by_scan_alike() {
        let dir = tempfile::tempdir().expect("tempdir");
        let now = vkey::unix_now_ms();
        let day_dir = dir.path().join("bodies").join(day_partition(now));

        let wanted = RequestBodies {
            request_id: "req-target".into(),
            ts: now,
            input: "{\"messages\":[{\"role\":\"user\",\"content\":\"hello\"}]}".into(),
            output: "{\"choices\":[{\"message\":{\"content\":\"hi\"}}]}".into(),
            truncated: false,
        };
        // Spread across batches, so a lookup that guessed the file would
        // have to be right about which one.
        for seq in 0..8u64 {
            let mut batch: Vec<RequestBodies> = (0..4)
                .map(|i| RequestBodies {
                    request_id: format!("req-{seq}-{i}"),
                    ts: now,
                    input: "{}".into(),
                    output: "{}".into(),
                    truncated: false,
                })
                .collect();
            if seq == 5 {
                batch.push(wanted.clone());
            }
            write_bodies(dir.path(), "node-a", seq, &batch).expect("bodies write");
        }

        let pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        let by_index = pipeline
            .bodies_for("req-target", now)
            .expect("found via the index");
        assert_eq!(by_index.input, wanted.input);
        assert_eq!(by_index.output, wanted.output);
        assert!(bodies_index_lookup(&day_dir, "req-target").is_some());

        // A partition that predates the index: same answer, slower.
        std::fs::remove_file(day_dir.join(BODIES_INDEX)).expect("drop the index");
        let by_scan = pipeline
            .bodies_for("req-target", now)
            .expect("found via the scan");
        assert_eq!(by_scan.input, wanted.input);
        assert!(pipeline.bodies_for("req-absent", now).is_none());
    }

    /// A request that has only just been served is the one most likely
    /// to be clicked, and it has not been flushed yet.
    #[test]
    fn a_just_served_request_is_readable_before_it_reaches_disk() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut pipeline = UsagePipeline::for_test(Some(dir.path().to_path_buf()));
        pipeline.capture = router_core::config::BodyCapture::All;
        pipeline.body_limit = 4096;
        let now = vkey::unix_now_ms();

        pipeline.record_bodies("req-live", now, 200, "{\"in\":1}", "{\"out\":2}");
        let found = pipeline
            .bodies_for("req-live", now)
            .expect("readable straight away");
        assert_eq!(found.input, "{\"in\":1}");
        assert_eq!(found.output, "{\"out\":2}");
        // Nothing was written: the flusher never ran.
        assert!(!dir.path().join("bodies").exists());
    }

    /// The hot cache is bounded, so a long session of scrolling the log
    /// cannot grow it without limit.
    #[test]
    fn the_hot_body_cache_evicts_oldest_first() {
        let mut hot = HotBodies::default();
        for i in 0..HOT_BODIES_CAP + 10 {
            hot.insert(RequestBodies {
                request_id: format!("req-{i}"),
                ts: 0,
                input: String::new(),
                output: String::new(),
                truncated: false,
            });
        }
        assert_eq!(hot.by_id.len(), HOT_BODIES_CAP);
        assert!(hot.get("req-0").is_none(), "the oldest should be gone");
        assert!(hot.get(&format!("req-{}", HOT_BODIES_CAP + 9)).is_some());
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
            ..Default::default()
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
            recent_rollups: Arc::default(),
            hot_bodies: Mutex::new(HotBodies::default()),
            dropped: AtomicU64::new(0),
            per_key_metrics: false,
            key_label_cap: 0,
            body_tx: Mutex::new(None),
            capture: BodyCapture::Off,
            body_limit: 0,
            trace_keys: BTreeSet::new(),
            trace_value_chars: 128,
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
            prompt: None,
            meta: BTreeMap::new(),
            error_class: None,
            seat: None,
            ttft_ms: None,
            queue_lag_ms: None,
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
            prompt: None,
            meta: BTreeMap::new(),
            error_class: None,
            seat: None,
            ttft_ms: None,
            queue_lag_ms: None,
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

    /// A node that keeps its data dir across a restart writes under a
    /// new identity but still holds the old files. Both are read from
    /// local disk, so the fleet read has to recognise both shipped
    /// objects as its own — otherwise every pre-restart day is counted
    /// twice.
    #[test]
    fn a_restart_does_not_make_this_nodes_own_rollups_look_foreign() {
        let dir = tempfile::tempdir().unwrap();
        let now = vkey::unix_now_ms();
        let rec = record(now, "openai", "gpt-4o-mini", Some("k1"), 200);
        let batch = std::slice::from_ref(&rec);
        write_rollups(dir.path(), "node-before", 0, &roll_up(batch)).unwrap();
        write_rollups(dir.path(), "node-after", 1, &roll_up(batch)).unwrap();

        let local = local_rollup_files(&dir.path().join("rollup"));
        assert_eq!(local.len(), 2);

        // The keys `ship_partitions` uploads for those same two files.
        let day = day_partition(now);
        for node in ["node-before", "node-after"] {
            let seq = if node == "node-before" { 0 } else { 1 };
            let key = format!("rollup/{day}/node={node}/{node}-{seq:08}.jsonl.zst");
            let mut parts = key.split('/');
            let (_, day_part, _, file) = (
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
                parts.next().unwrap(),
            );
            assert!(
                local.contains(&(day_part.to_owned(), file.to_owned())),
                "{key} should be recognised as already read from local disk",
            );
        }
    }
}

#[cfg(test)]
mod trace_tests {
    use super::*;

    fn keys() -> BTreeSet<String> {
        router_core::config::DEFAULT_TRACE_KEYS
            .iter()
            .map(|k| (*k).to_owned())
            .collect()
    }

    /// The shape LiteLLM-based callers send: everything nested under
    /// `metadata.trace_metadata`, with Langfuse's own field names beside
    /// it. This is the majority of production traffic, so it is the case
    /// that must not regress.
    #[test]
    fn reads_the_nested_langfuse_shape() {
        let body = r#"{
            "model": "gpt-5",
            "messages": [],
            "metadata": {
                "trace_name": "agentic_dag_coder",
                "trace_user_id": "org-uuid",
                "session_id": "org-uuid_20250107000002",
                "tags": ["orgId:org-uuid", "agno", "temporal"],
                "trace_metadata": {
                    "org_id": "org-uuid",
                    "chart_id": "20250107000002",
                    "workflow_id": "WORKFLOW_HCC_CONFIRMED",
                    "batch_id": "20241015600018",
                    "service": "agentic_dag_coder",
                    "generation_name": "icd_coder",
                    "event_processing_tag": "ICD_EXTRACTION",
                    "env": "prod",
                    "agent": "icd_coder"
                }
            }
        }"#;
        let info = trace_info(body, &keys(), 128);
        assert_eq!(info.dims["workflow_id"], "WORKFLOW_HCC_CONFIRMED");
        assert_eq!(info.dims["chart_id"], "20250107000002");
        assert_eq!(info.dims["org_id"], "org-uuid");
        assert_eq!(info.dims["batch_id"], "20241015600018");
        assert_eq!(info.dims["service"], "agentic_dag_coder");
        assert_eq!(info.dims["agent"], "icd_coder");
        // Renamed on the way in, so one filter works across both client
        // shapes rather than one per caller vocabulary.
        assert_eq!(info.dims["stage"], "ICD_EXTRACTION");
        assert_eq!(info.dims["generation"], "icd_coder");
        assert_eq!(info.dims["env"], "prod");
        // Redundant spellings are dropped rather than stored twice.
        assert!(!info.dims.contains_key("trace_name"));
        assert!(!info.dims.contains_key("session_id"));
        assert!(!info.dims.contains_key("tags"));
        assert!(!info.dims.contains_key("trace_user_id"));
    }

    /// The shape clients talking to the gateway directly send: the same
    /// keys, flat.
    #[test]
    fn reads_the_flat_shape() {
        let body = r#"{"metadata":{"service":"agentic_dag_coder","agent":"icd_coder",
            "org_id":"org-1","chart_id":"chart-1","workflow_id":"wf-1"}}"#;
        let info = trace_info(body, &keys(), 128);
        assert_eq!(info.dims["workflow_id"], "wf-1");
        assert_eq!(info.dims["chart_id"], "chart-1");
        assert_eq!(info.dims["agent"], "icd_coder");
    }

    /// Only what config allows becomes a dimension. A caller can put
    /// anything under `metadata`; none of it may become a log column on
    /// its own say-so.
    #[test]
    fn an_unlisted_key_is_not_a_dimension() {
        let body = r#"{"metadata":{"workflow_id":"wf-1","patient_id":"p-1","whatever":"x"}}"#;
        let info = trace_info(body, &keys(), 128);
        assert_eq!(info.dims["workflow_id"], "wf-1");
        assert!(!info.dims.contains_key("patient_id"));
        assert!(!info.dims.contains_key("whatever"));

        // …and adding it to config is all it takes.
        let mut extended = keys();
        extended.insert("patient_id".into());
        let info = trace_info(body, &extended, 128);
        assert_eq!(info.dims["patient_id"], "p-1");
        assert!(!info.dims.contains_key("whatever"));
    }

    /// A container under an allowed key is a caller mistake; flattening
    /// one would put unbounded text into a filter chip.
    #[test]
    fn non_scalar_and_empty_values_are_skipped() {
        let body = r#"{"metadata":{"workflow_id":{"nested":"object"},"chart_id":["a"],
            "agent":"   ","service":null,"stage":"","org_id":42,"env":true}}"#;
        let info = trace_info(body, &keys(), 128);
        assert!(!info.dims.contains_key("workflow_id"));
        assert!(!info.dims.contains_key("chart_id"));
        assert!(!info.dims.contains_key("agent"));
        assert!(!info.dims.contains_key("service"));
        assert!(!info.dims.contains_key("stage"));
        // Numbers and bools are scalars and do render — some callers
        // send numeric ids unquoted.
        assert_eq!(info.dims["org_id"], "42");
        assert_eq!(info.dims["env"], "true");
    }

    /// A value long enough to matter is cut, not stored.
    #[test]
    fn values_are_length_capped() {
        let long = "x".repeat(500);
        let body = format!(r#"{{"metadata":{{"chart_id":"{long}"}}}}"#);
        let info = trace_info(&body, &keys(), 32);
        // 32 characters plus the ellipsis the truncator marks it with.
        assert_eq!(info.dims["chart_id"].chars().count(), 33);
    }

    /// Attribution is a side channel: nothing it is handed may fail a
    /// request or produce a partial record.
    #[test]
    fn junk_bodies_yield_nothing_rather_than_failing() {
        for body in [
            "",
            "not json at all",
            "{",
            "[]",
            "null",
            r#"{"messages":[]}"#,
            r#"{"metadata":null}"#,
            r#"{"metadata":"a string"}"#,
            r#"{"metadata":[1,2,3]}"#,
        ] {
            let info = trace_info(body, &keys(), 128);
            assert!(info.dims.is_empty(), "body {body:?} produced dimensions");
            assert!(info.event_create_ts.is_none());
        }
    }

    /// An empty allowlist turns the whole feature off.
    #[test]
    fn no_configured_keys_means_no_dimensions() {
        let body = r#"{"metadata":{"workflow_id":"wf-1"}}"#;
        let info = trace_info(body, &BTreeSet::new(), 128);
        assert!(info.dims.is_empty());
    }

    /// `event_create_ts` is read for the lag measurement even though it
    /// is not a filterable dimension — the two are deliberately separate.
    #[test]
    fn the_source_timestamp_is_read_without_being_a_dimension() {
        let body = r#"{"metadata":{"trace_metadata":{"event_create_ts":"1700000000000"}}}"#;
        let info = trace_info(body, &keys(), 128);
        assert_eq!(info.event_create_ts.as_deref(), Some("1700000000000"));
        assert!(!info.dims.contains_key("event_create_ts"));
    }

    #[test]
    fn queue_lag_reads_seconds_and_milliseconds_alike() {
        let now = 1_700_000_600_000u64;
        assert_eq!(queue_lag_ms(Some("1700000000000"), now), Some(600_000));
        // The same instant in seconds.
        assert_eq!(queue_lag_ms(Some("1700000000"), now), Some(600_000));
        assert_eq!(queue_lag_ms(Some(" 1700000000 "), now), Some(600_000));
    }

    /// A misparsed unit yields a lag of decades rather than an obvious
    /// error, so implausible stamps are refused outright.
    #[test]
    fn implausible_or_skewed_timestamps_are_refused() {
        let now = 1_700_000_600_000u64;
        for stamp in ["0", "12345", "not a number", "", "-1", "1.5"] {
            assert_eq!(queue_lag_ms(Some(stamp), now), None, "stamp {stamp:?}");
        }
        // A caller whose clock runs ahead of this box: a negative lag is
        // noise, not a measurement.
        assert_eq!(queue_lag_ms(Some("1700000900000"), now), None);
        assert_eq!(queue_lag_ms(None, now), None);
    }

    /// A filter narrows on every term it names, and a term no record
    /// carries matches nothing rather than being ignored.
    #[test]
    fn a_meta_filter_is_conjunctive() {
        let mut rec = UsageRecord {
            ts: 1_700_000_000_000,
            request_id: "r".into(),
            endpoint: "chat".into(),
            requested: "gpt-5".into(),
            provider: "openai".into(),
            model: "gpt-5".into(),
            vkey: None,
            status: 200,
            stream: false,
            input_tokens: 1,
            output_tokens: 1,
            cached_tokens: 0,
            cost_micro_usd: 1,
            latency_ms: 1,
            overhead_us: 1,
            attempts: 1,
            tag: None,
            prompt: None,
            meta: BTreeMap::new(),
            error_class: None,
            seat: None,
            ttft_ms: None,
            queue_lag_ms: None,
        };
        rec.meta
            .insert("workflow_id".into(), "WORKFLOW_RISE_HCS".into());
        rec.meta.insert("stage".into(), "ICD_SEARCH".into());

        let matching = HistoryFilter {
            meta: vec![
                ("workflow_id".into(), "WORKFLOW_RISE_HCS".into()),
                ("stage".into(), "ICD_SEARCH".into()),
            ],
            ..Default::default()
        };
        assert!(matching.matches(&rec));
        assert!(matching.needs_records());

        // One term right, one wrong.
        let partial = HistoryFilter {
            meta: vec![
                ("workflow_id".into(), "WORKFLOW_RISE_HCS".into()),
                ("stage".into(), "CPT_SEARCH".into()),
            ],
            ..Default::default()
        };
        assert!(!partial.matches(&rec));

        // A dimension this record does not carry at all.
        let absent = HistoryFilter {
            meta: vec![("agent".into(), "icd_coder".into())],
            ..Default::default()
        };
        assert!(!absent.matches(&rec));

        // And an unconstrained filter still needs no record scan.
        assert!(!HistoryFilter::default().needs_records());
    }
}
