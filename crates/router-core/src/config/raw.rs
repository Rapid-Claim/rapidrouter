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
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RawServer {
    pub host: String,
    pub port: u16,
    pub max_body_size_mb: u64,
    pub auth_keys: Vec<String>,
    pub drain_timeout_secs: u64,
}

impl Default for RawServer {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 8080,
            max_body_size_mb: 50,
            auth_keys: Vec::new(),
            drain_timeout_secs: 30,
        }
    }
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
