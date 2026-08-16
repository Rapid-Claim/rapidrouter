//! Serde-facing config types: the document as written, before validation
//! and secret resolution. `deny_unknown_fields` everywhere — an unknown
//! field is a typo, and typos in a gateway config must fail loudly.

use std::collections::BTreeMap;

use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawConfig {
    pub server: RawServer,
    pub providers: BTreeMap<String, RawProvider>,
    pub aliases: BTreeMap<String, String>,
    pub fallbacks: BTreeMap<String, Vec<String>>,
    pub reliability: RawReliability,
    /// File-mode virtual keys (hash form) — GitOps shops declare them
    /// here; managed mode keeps them in the store instead.
    pub virtual_keys: Vec<RawVirtualKey>,
    pub console: RawConsole,
    pub store: RawStore,
    pub usage: RawUsage,
    /// Price overrides per `provider/model` (USD per million tokens).
    pub pricing: BTreeMap<String, RawPrice>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawServer {
    pub host: String,
    pub port: u16,
    pub max_body_size_mb: u64,
    pub auth_keys: Vec<String>,
    /// Refuse anonymous data-plane requests even when `auth_keys` is
    /// empty (the multi-tenant posture together with virtual keys).
    pub require_auth: bool,
    pub drain_timeout_secs: u64,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            max_body_size_mb: 100,
            auth_keys: Vec::new(),
            require_auth: false,
            drain_timeout_secs: 30,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawVirtualKey {
    pub name: String,
    pub id: String,
    /// `blake3:<hex>` — the file never carries secret material.
    pub secret_hash: String,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default)]
    pub budget: Option<RawBudget>,
    #[serde(default)]
    pub rate_limit: Option<RawRateLimit>,
    /// RFC 3339 UTC, e.g. `2027-01-01T00:00:00Z`.
    #[serde(default)]
    pub expires: Option<String>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_enabled() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawBudget {
    pub usd: f64,
    pub period: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRateLimit {
    #[serde(default)]
    pub rpm: Option<u64>,
    #[serde(default)]
    pub tpm: Option<u64>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawConsole {
    /// Admin credentials; the console and `/admin/api/*` exist only when
    /// at least one is configured.
    pub admin_keys: Vec<String>,
    pub session_ttl_secs: u64,
}

impl Default for RawConsole {
    fn default() -> Self {
        Self {
            admin_keys: Vec::new(),
            session_ttl_secs: 12 * 60 * 60,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawStore {
    /// `file`, `s3`, or `dynamodb`. Defaults to `file`.
    pub backend: String,
    /// S3 bucket and key prefix.
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    /// DynamoDB table.
    pub table: Option<String>,
    /// AWS region and endpoint override; absent means the ambient config.
    pub region: Option<String>,
    pub endpoint: Option<String>,
    /// How often to poll the store for changes another node made.
    pub refresh_interval_secs: u64,
    /// How often this node announces itself.
    pub heartbeat_interval_secs: u64,
    /// How long after its last heartbeat a node still counts as live.
    pub liveness_window_secs: u64,
}

impl Default for RawStore {
    fn default() -> Self {
        Self {
            backend: "file".into(),
            bucket: None,
            prefix: None,
            table: None,
            region: None,
            endpoint: None,
            refresh_interval_secs: 3,
            heartbeat_interval_secs: 5,
            liveness_window_secs: 15,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawUsage {
    pub retention_days: u32,
    pub flush_interval_secs: u64,
    /// Emit bounded per-key metrics labels (`vkey=…`).
    pub per_key_metrics: bool,
}

impl Default for RawUsage {
    fn default() -> Self {
        Self {
            retention_days: 30,
            flush_interval_secs: 10,
            per_key_metrics: false,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPrice {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawProvider {
    /// Adapter override for providers that are not well-known names.
    /// Currently the only accepted value is `"openai_compat"`.
    pub r#type: Option<String>,
    pub base_url: Option<String>,
    /// `"bearer"` (default for API providers) or `"none"` (local servers).
    pub auth: Option<String>,
    pub keys: Vec<RawKey>,
    pub max_concurrency: usize,
    pub timeout_secs: u64,
    /// Azure only.
    pub endpoint: Option<String>,
    /// Azure only.
    pub api_version: Option<String>,
    /// Azure only: model name -> deployment name.
    pub deployments: BTreeMap<String, String>,
    /// Bedrock only.
    pub region: Option<String>,
    /// Bedrock only: `env.*` reference or literal AWS access key id.
    pub access_key_id: Option<String>,
    /// Vertex only.
    pub project: Option<String>,
    /// Vertex only; defaults to `us-central1`.
    pub location: Option<String>,
    /// Codex subscription only.
    pub codex: Option<RawCodex>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawCodex {
    /// Codex CLI version string sent as `Version`/`User-Agent`.
    pub version: Option<String>,
    /// `none`|`minimal`|`low`|`medium`|`high`|`xhigh`|`max`, or `""` to
    /// send no floor and take the backend's per-model default.
    pub reasoning_effort: Option<String>,
    /// `low`|`medium`|`high`, or `""` for the backend default.
    pub verbosity: Option<String>,
}

impl Default for RawProvider {
    fn default() -> Self {
        Self {
            r#type: None,
            base_url: None,
            auth: None,
            keys: Vec::new(),
            max_concurrency: 512,
            timeout_secs: 120,
            endpoint: None,
            api_version: None,
            deployments: BTreeMap::new(),
            region: None,
            access_key_id: None,
            project: None,
            location: None,
            codex: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawKey {
    pub name: String,
    /// `env.VAR_NAME` reference, or a literal value (tests, throwaways).
    pub value: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Restrict this key to specific models; omitted = all models.
    #[serde(default)]
    pub models: Option<Vec<String>>,
    /// This key's own ceiling, independent of any virtual key's.
    ///
    /// Provider accounts are rate limited per credential, so the limit
    /// belongs on the credential: one key hitting its ceiling should move
    /// traffic to the next key, not fail the request.
    #[serde(default)]
    pub rpm: Option<u64>,
    #[serde(default)]
    pub tpm: Option<u64>,
}

fn default_weight() -> f64 {
    1.0
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawReliability {
    pub breaker: RawBreaker,
    pub retries: RawRetries,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawBreaker {
    pub failure_threshold: u32,
    pub window_secs: u64,
    pub cooldown_secs: u64,
}

impl Default for RawBreaker {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            window_secs: 30,
            cooldown_secs: 15,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawRetries {
    pub max_attempts: u32,
    pub on: Vec<String>,
}

impl Default for RawRetries {
    fn default() -> Self {
        Self {
            max_attempts: 2,
            on: vec!["connect_error".into(), "429".into(), "5xx".into()],
        }
    }
}
