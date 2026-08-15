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
    pub reliability: Reliability,
    /// File-declared virtual keys; managed mode appends store-held keys.
    pub virtual_keys: Vec<crate::vkey::VirtualKeyDef>,
    pub console: ConsoleConfig,
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

#[derive(Debug, Clone)]
pub struct UsageConfig {
    pub retention_days: u32,
    pub flush_interval: Duration,
    pub per_key_metrics: bool,
}

impl Default for UsageConfig {
    fn default() -> Self {
        Self {
            retention_days: 30,
            flush_interval: Duration::from_secs(10),
            per_key_metrics: false,
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
}

#[derive(Debug)]
pub struct ApiKey {
    pub name: String,
    pub secret: SecretString,
    pub weight: f64,
    pub models: Option<Vec<String>>,
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
