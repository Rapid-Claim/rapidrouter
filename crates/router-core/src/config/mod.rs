//! Configuration: loading, total validation, and secret resolution.
//!
//! Validation is *total*: every problem in the document is reported with a
//! path (`providers.groq.keys[0].weight: must be > 0`), and nothing binds a
//! port until the whole document is clean. Secret values resolve to
//! [`SecretString`] at load; raw key material never lives in these types as
//! plain `String`s.

pub mod presets;
mod raw;
mod validate;

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::time::Duration;

use crate::secret::SecretString;

pub use raw::RawConfig;
pub use validate::EnvSource;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    OpenAi,
    Anthropic,
    Gemini,
    Azure,
    Bedrock,
    /// Gemini dialect over Vertex AI's project/location endpoints.
    Vertex,
    OpenAiCompat,
    /// Anthropic's Messages API reached with a Claude Code subscription
    /// OAuth token instead of a metered API key.
    ClaudeSubscription,
    /// The private Responses endpoint the Codex CLI talks to, reached
    /// with a ChatGPT subscription credential.
    CodexSubscription,
}

impl ProviderKind {
    /// Whether this provider is backed by a subscription seat rather than
    /// a metered API key. Seats rotate their credentials, are benched on
    /// the provider's own schedule, and carry vendor-specific headers.
    pub fn is_subscription(self) -> bool {
        matches!(self, Self::ClaudeSubscription | Self::CodexSubscription)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthMode {
    /// Provider-appropriate key auth (bearer header, api-key header, etc.).
    Key,
    /// No authentication (local servers).
    None,
}

#[derive(Debug)]
pub struct Config {
    pub server: ServerConfig,
    pub providers: BTreeMap<String, Provider>,
    /// Alias name -> fully resolved target.
    pub aliases: BTreeMap<String, TargetModel>,
    /// Fully resolved fallback chains.
    pub fallbacks: BTreeMap<TargetModel, Vec<TargetModel>>,
    /// Routing groups by name, each a weighted primary and fallback pool.
    pub groups: BTreeMap<String, RoutingGroup>,
    pub reliability: Reliability,
    /// File-declared virtual keys; managed mode appends store-held keys.
    pub virtual_keys: Vec<crate::vkey::VirtualKeyDef>,
    pub console: ConsoleConfig,
    pub store: StoreConfig,
    pub usage: UsageConfig,
    /// Price overrides per `provider/model`, merged over the built-ins.
    pub pricing: BTreeMap<String, Price>,
}

#[derive(Debug)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub max_body_size: u64,
    pub auth_keys: Vec<SecretString>,
    pub require_auth: bool,
    pub drain_timeout: Duration,
}

#[derive(Debug, Default)]
pub struct ConsoleConfig {
    pub admin_keys: Vec<SecretString>,
    pub session_ttl: Duration,
}

impl ConsoleConfig {
    /// The console and admin API exist only when credentials do.
    pub fn enabled(&self) -> bool {
        !self.admin_keys.is_empty()
    }
}

/// Read only the `[store]` section, without resolving any `env.*` or
/// `store.*` reference.
///
/// Startup is circular otherwise: the full config may name secrets that
/// live in the store, but opening the store requires knowing where it is.
/// The store section is deliberately restricted to literals so it can be
/// read first.
pub fn store_section(text: &str, format: Format) -> Result<StoreConfig, LoadError> {
    let raw: raw::RawConfig = match format {
        Format::Toml => toml::from_str(text).map_err(|e| LoadError::Parse(e.to_string()))?,
        Format::Json => serde_json::from_str(text).map_err(|e| LoadError::Parse(e.to_string()))?,
    };
    Ok(StoreConfig {
        backend: raw.store.backend,
        bucket: raw.store.bucket,
        prefix: raw.store.prefix,
        table: raw.store.table,
        region: raw.store.region,
        endpoint: raw.store.endpoint,
        refresh_interval_secs: raw.store.refresh_interval_secs,
        heartbeat_interval_secs: raw.store.heartbeat_interval_secs,
        liveness_window_secs: raw.store.liveness_window_secs,
    })
}

/// Where control-plane state lives. Every node pointed at the same
/// backend is a node of the same fleet; there is no membership beyond
/// that, and nothing to join.
#[derive(Debug, Clone)]
pub struct StoreConfig {
    pub backend: String,
    pub bucket: Option<String>,
    pub prefix: Option<String>,
    pub table: Option<String>,
    pub region: Option<String>,
    pub endpoint: Option<String>,
    pub refresh_interval_secs: u64,
    pub heartbeat_interval_secs: u64,
    pub liveness_window_secs: u64,
}

impl Default for StoreConfig {
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

impl StoreConfig {
    /// Whether more than one node can reach this store. Shared stores
    /// require a cluster-wide secret key; a local file does not.
    pub fn is_shared(&self) -> bool {
        matches!(self.backend.as_str(), "s3" | "dynamodb")
    }
}

#[derive(Debug, Clone)]
pub struct UsageConfig {
    pub retention_days: u32,
    pub flush_interval: Duration,
    pub per_key_metrics: bool,
    /// Whether request and response bodies are kept for inspection.
    ///
    /// On by default: being able to open a request and read what was
    /// actually sent is the difference between a usage log and a
    /// debugging tool. Two things follow from that and are handled
    /// rather than assumed away — bodies are two orders of magnitude
    /// larger than the metadata, so they are written to their own stream
    /// and never touched by a log listing; and they contain whatever
    /// callers send, so anyone routing regulated data should know these
    /// are stored.
    pub capture_bodies: BodyCapture,
    /// Bytes kept per body. Longer ones are truncated with a marker
    /// rather than dropped: the head of a prompt identifies the request,
    /// and one pathological caller should not be able to fill the disk.
    pub body_limit_bytes: usize,
    /// How long captured bodies live. Defaults to the same window as the
    /// metadata they belong to, so a request that appears in the log can
    /// always be opened.
    pub body_retention_days: u32,
}

/// Which requests keep their bodies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BodyCapture {
    Off,
    /// Only requests that failed — the debugging case, at a fraction of
    /// the volume and usually without a successful answer to store.
    Errors,
    #[default]
    All,
}

impl BodyCapture {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" | "none" | "false" => Some(Self::Off),
            "errors" => Some(Self::Errors),
            "all" | "true" => Some(Self::All),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Errors => "errors",
            Self::All => "all",
        }
    }

    pub fn wants(self, status: u16) -> bool {
        match self {
            Self::Off => false,
            Self::Errors => status >= 400,
            Self::All => true,
        }
    }
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            retention_days: 30,
            flush_interval: Duration::from_secs(10),
            per_key_metrics: false,
            capture_bodies: BodyCapture::All,
            body_limit_bytes: 256 * 1024,
            body_retention_days: 30,
        }
    }
}

/// USD per million tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Price {
    pub input_per_mtok: f64,
    pub output_per_mtok: f64,
}

#[derive(Debug)]
pub struct Provider {
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub auth: AuthMode,
    pub keys: Vec<ApiKey>,
    pub max_concurrency: usize,
    pub timeout: Duration,
    pub azure: Option<AzureSettings>,
    pub bedrock: Option<BedrockSettings>,
    pub vertex: Option<VertexSettings>,
    pub codex: Option<CodexSettings>,
}

/// Per-provider knobs for the Codex subscription transport.
#[derive(Debug, Clone)]
pub struct CodexSettings {
    /// The Codex CLI version presented to the backend. Configurable
    /// because the backend gates model families on it and an operator
    /// must be able to follow a new family without a gateway release.
    pub version: String,
    /// Reasoning-depth floor; `None` restores the backend's own default.
    pub reasoning_effort: Option<String>,
    /// Output-verbosity floor; `None` restores the backend's own default.
    pub verbosity: Option<String>,
    /// Resolution for rasterizing an attached PDF.
    ///
    /// This backend accepts no document part at all — its own client's
    /// content vocabulary is text and images and nothing else — so a PDF
    /// is rendered to one image per page before translation. 150 DPI puts
    /// a Letter page a little past the resolution vision models downsample
    /// to, so raising it costs request bytes without adding detail.
    pub pdf_dpi: u32,
    /// Ceiling on pages rendered from one attached PDF. Pages beyond it
    /// are reported, never silently cut.
    pub pdf_max_pages: usize,
}

impl Default for CodexSettings {
    fn default() -> Self {
        Self {
            version: "0.146.0".into(),
            reasoning_effort: Some("xhigh".into()),
            verbosity: Some("low".into()),
            pdf_dpi: 150,
            pdf_max_pages: 50,
        }
    }
}

#[derive(Debug)]
pub struct ApiKey {
    pub name: String,
    pub secret: SecretString,
    pub weight: f64,
    pub models: Option<Vec<String>>,
    /// This key's own request/token ceilings; see [`raw::RawKey`].
    pub rpm: Option<u64>,
    pub tpm: Option<u64>,
    /// Where this credential was read from, when it came from a file.
    ///
    /// Retained only for `file:` references — a subscription seat whose
    /// token rotates needs somewhere to write the new one. An inline or
    /// `env.` value has no path, and for an inline value the reference
    /// *is* the secret, so nothing is kept.
    pub source_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AzureSettings {
    pub endpoint: String,
    pub api_version: String,
    /// Model name -> deployment name.
    pub deployments: BTreeMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct BedrockSettings {
    pub region: String,
    pub access_key_id: String,
}

#[derive(Debug, Clone)]
pub struct VertexSettings {
    pub project: String,
    pub location: String,
}

#[derive(Debug)]
pub struct Reliability {
    pub breaker: Breaker,
    pub retries: Retries,
}

#[derive(Debug)]
pub struct Breaker {
    pub failure_threshold: u32,
    pub window: Duration,
    pub cooldown: Duration,
}

#[derive(Debug, Clone)]
pub struct Retries {
    pub max_attempts: u32,
    pub on: Vec<RetryOn>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryOn {
    ConnectError,
    Status429,
    Status5xx,
}

/// A `provider/model` pair, the unit routing and fallbacks speak in.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TargetModel {
    pub provider: String,
    pub model: String,
}

impl TargetModel {
    pub fn parse(s: &str) -> Option<Self> {
        let (provider, model) = s.split_once('/')?;
        if provider.is_empty() || model.is_empty() {
            return None;
        }
        Some(Self {
            provider: provider.to_owned(),
            model: model.to_owned(),
        })
    }
}

impl fmt::Display for TargetModel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)
    }
}

/// A named pool of models callers reach by sending the group's name as
/// the model id.
///
/// The two pools answer different questions. `primary` is a *split*: the
/// gateway picks one of its members per request, in proportion to weight,
/// so weights are how traffic is apportioned across providers. `fallback`
/// is a *reserve*: nothing in it serves traffic while any primary member
/// can, and its weights only decide the order in which the reserve is
/// drawn on.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RoutingGroup {
    pub primary: Vec<WeightedTarget>,
    pub fallback: Vec<WeightedTarget>,
}

impl RoutingGroup {
    /// Every target in the group, primary first — for the surfaces that
    /// only need to know which models a group can reach.
    pub fn targets(&self) -> impl Iterator<Item = &TargetModel> {
        self.primary.iter().chain(&self.fallback).map(|w| &w.target)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WeightedTarget {
    pub target: TargetModel,
    pub weight: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub path: String,
    pub message: String,
}

impl ConfigError {
    pub fn new(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.path, self.message)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to read config: {0}")]
    Io(#[from] std::io::Error),
    #[error("failed to parse config: {0}")]
    Parse(String),
    #[error("invalid config:\n{}", format_errors(.0))]
    Invalid(Vec<ConfigError>),
}

fn format_errors(errors: &[ConfigError]) -> String {
    errors
        .iter()
        .map(|e| format!("  - {e}"))
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Toml,
    Json,
}

impl Config {
    /// Load and validate a config file; format chosen by extension
    /// (`.json` -> JSON, anything else -> TOML).
    pub fn load(path: &Path) -> Result<Self, LoadError> {
        Self::load_with_env(path, &std_env)
    }

    pub fn load_with_env(path: &Path, env: &dyn EnvSource) -> Result<Self, LoadError> {
        let text = std::fs::read_to_string(path)?;
        let format = match path.extension().and_then(|e| e.to_str()) {
            Some("json") => Format::Json,
            _ => Format::Toml,
        };
        Self::from_str_with_env(&text, format, env)
    }

    pub fn from_str(text: &str, format: Format) -> Result<Self, LoadError> {
        Self::from_str_with_env(text, format, &std_env)
    }

    pub fn from_str_with_env(
        text: &str,
        format: Format,
        env: &dyn EnvSource,
    ) -> Result<Self, LoadError> {
        let raw: RawConfig = match format {
            Format::Toml => toml::from_str(text).map_err(|e| LoadError::Parse(e.to_string()))?,
            Format::Json => {
                serde_json::from_str(text).map_err(|e| LoadError::Parse(e.to_string()))?
            }
        };
        validate::validate(raw, env).map_err(LoadError::Invalid)
    }

    /// Zero-config startup: build a config from whichever conventional
    /// provider env vars are present. Returns `None` if nothing was found.
    pub fn discover_from_env(env: &dyn EnvSource) -> Option<Self> {
        let mut doc = String::new();
        for &name in presets::DISCOVERY_ORDER {
            let var = presets::preset(name)
                .and_then(|p| p.discovery_env)
                .expect("discovery list entries define an env var");
            if env.get(var).is_some_and(|v| !v.is_empty()) {
                doc.push_str(&format!(
                    "[providers.{name}]\nkeys = [{{ name = \"default\", value = \"env.{var}\" }}]\n\n"
                ));
            }
        }
        // Databricks needs both a workspace host and a token.
        if let (Some(host), Some(_)) = (env.get("DATABRICKS_HOST"), env.get("DATABRICKS_TOKEN")) {
            let host = host.trim_end_matches('/');
            doc.push_str(&format!(
                "[providers.databricks]\nbase_url = \"{host}/serving-endpoints\"\nkeys = [{{ name = \"default\", value = \"env.DATABRICKS_TOKEN\" }}]\n\n"
            ));
        }
        if doc.is_empty() {
            return None;
        }
        Some(
            Self::from_str_with_env(&doc, Format::Toml, env)
                .expect("generated discovery config is valid"),
        )
    }
}

impl Config {
    pub fn retries(&self) -> &Retries {
        &self.reliability.retries
    }
}

fn std_env(var: &str) -> Option<String> {
    std::env::var(var).ok()
}
