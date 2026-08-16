//! Total validation of a raw config document into the resolved [`Config`].
//! Collects every error with its document path rather than failing fast.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::presets::preset;
use super::raw::{RawConfig, RawProvider};
use super::{
    ApiKey, AuthMode, AzureSettings, BedrockSettings, Breaker, CodexSettings, Config, ConfigError,
    Provider, ProviderKind, Reliability, Retries, RetryOn, ServerConfig, TargetModel,
    VertexSettings,
};
use crate::secret::SecretString;

/// Source of environment values, injectable so tests never touch process
/// state (and never race each other over `set_var`).
pub trait EnvSource {
    fn get(&self, var: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String>> EnvSource for F {
    fn get(&self, var: &str) -> Option<String> {
        self(var)
    }
}

pub(super) fn validate(raw: RawConfig, env: &dyn EnvSource) -> Result<Config, Vec<ConfigError>> {
    let mut errors = Vec::new();

    let server = validate_server(&raw, env, &mut errors);
    let providers = validate_providers(&raw, env, &mut errors);
    let aliases = validate_aliases(&raw, &providers, &mut errors);
    let fallbacks = validate_fallbacks(&raw, &providers, &aliases, &mut errors);
    let reliability = validate_reliability(&raw, &mut errors);
    let virtual_keys = validate_virtual_keys(&raw, &providers, &aliases, &mut errors);
    let console = validate_console(&raw, env, &mut errors);
    let store = validate_store(&raw, &mut errors);
    let usage = validate_usage(&raw, &mut errors);
    let pricing = validate_pricing(&raw, &mut errors);

    if errors.is_empty() {
        Ok(Config {
            server,
            providers,
            aliases,
            fallbacks,
            reliability,
            virtual_keys,
            console,
            store,
            usage,
            pricing,
        })
    } else {
        Err(errors)
    }
}

fn validate_server(
    raw: &RawConfig,
    env: &dyn EnvSource,
    errors: &mut Vec<ConfigError>,
) -> ServerConfig {
    let s = &raw.server;
    if s.max_body_size_mb == 0 || s.max_body_size_mb > 1024 {
        errors.push(ConfigError::new(
            "server.max_body_size_mb",
            "must be between 1 and 1024",
        ));
    }
    let mut auth_keys = Vec::new();
    for (i, value) in s.auth_keys.iter().enumerate() {
        match resolve_secret(value, env) {
            Ok(secret) => auth_keys.push(secret),
            Err(msg) => errors.push(ConfigError::new(format!("server.auth_keys[{i}]"), msg)),
        }
    }
    ServerConfig {
        host: s.host.clone(),
        port: s.port,
        max_body_size: s.max_body_size_mb * 1024 * 1024,
        auth_keys,
        require_auth: s.require_auth,
        drain_timeout: Duration::from_secs(s.drain_timeout_secs),
    }
}

fn validate_virtual_keys(
    raw: &RawConfig,
    providers: &BTreeMap<String, Provider>,
    aliases: &BTreeMap<String, TargetModel>,
    errors: &mut Vec<ConfigError>,
) -> Vec<crate::vkey::VirtualKeyDef> {
    use crate::vkey;

    let mut defs = Vec::new();
    let mut seen_ids = BTreeSet::new();
    for (i, rk) in raw.virtual_keys.iter().enumerate() {
        let path = format!("virtual_keys[{i}]");
        let before = errors.len();

        if rk.name.is_empty() {
            errors.push(ConfigError::new(
                format!("{path}.name"),
                "must not be empty",
            ));
        }
        if rk.id.len() != 6 || !rk.id.bytes().all(|b| b.is_ascii_hexdigit()) {
            errors.push(ConfigError::new(
                format!("{path}.id"),
                "must be 6 hex characters (the id segment of `ck-<id>-…`)",
            ));
        } else if !seen_ids.insert(rk.id.clone()) {
            errors.push(ConfigError::new(
                format!("{path}.id"),
                format!("duplicate key id `{}`", rk.id),
            ));
        }
        let hex = rk.secret_hash.strip_prefix("blake3:").unwrap_or("");
        if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            errors.push(ConfigError::new(
                format!("{path}.secret_hash"),
                "must be `blake3:` followed by 64 hex characters \
                 (create one with `rapid-router key hash`)",
            ));
        }
        for (j, scope) in rk.models.iter().enumerate() {
            let scope_path = format!("{path}.models[{j}]");
            if scope.is_empty() {
                errors.push(ConfigError::new(scope_path, "must not be empty"));
            } else if let Some((provider, _)) = scope.split_once('/') {
                if !providers.contains_key(provider) {
                    errors.push(ConfigError::new(
                        scope_path,
                        format!("unknown provider `{provider}`"),
                    ));
                }
            } else if !aliases.contains_key(scope) {
                errors.push(ConfigError::new(
                    scope_path,
                    format!("`{scope}` is not a configured alias (use `provider/model` to scope to a model)"),
                ));
            }
        }
        let budget = rk.budget.as_ref().and_then(|b| {
            let period = match b.period.as_str() {
                "daily" => Some(vkey::BudgetPeriod::Daily),
                "weekly" => Some(vkey::BudgetPeriod::Weekly),
                "monthly" => Some(vkey::BudgetPeriod::Monthly),
                other => {
                    errors.push(ConfigError::new(
                        format!("{path}.budget.period"),
                        format!("unknown period `{other}` (expected daily, weekly, or monthly)"),
                    ));
                    None
                }
            }?;
            if !(b.usd.is_finite() && b.usd > 0.0) {
                errors.push(ConfigError::new(
                    format!("{path}.budget.usd"),
                    "must be a positive number",
                ));
                return None;
            }
            Some(vkey::Budget { usd: b.usd, period })
        });
        let rate = rk.rate_limit.as_ref().map(|r| {
            if r.rpm == Some(0) {
                errors.push(ConfigError::new(
                    format!("{path}.rate_limit.rpm"),
                    "must be > 0",
                ));
            }
            if r.tpm == Some(0) {
                errors.push(ConfigError::new(
                    format!("{path}.rate_limit.tpm"),
                    "must be > 0",
                ));
            }
            if r.rpm.is_none() && r.tpm.is_none() {
                errors.push(ConfigError::new(
                    format!("{path}.rate_limit"),
                    "must set rpm and/or tpm",
                ));
            }
            vkey::RateLimit {
                rpm: r.rpm,
                tpm: r.tpm,
            }
        });
        let expires_ms = rk.expires.as_ref().and_then(|s| {
            let parsed = vkey::parse_rfc3339_utc_ms(s);
            if parsed.is_none() {
                errors.push(ConfigError::new(
                    format!("{path}.expires"),
                    "must be RFC 3339 UTC, e.g. `2027-01-01T00:00:00Z`",
                ));
            }
            parsed
        });

        if errors.len() == before {
            defs.push(vkey::VirtualKeyDef {
                id: rk.id.clone(),
                name: rk.name.clone(),
                secret_hash: rk.secret_hash.clone(),
                prev_secret: None,
                models: rk.models.clone(),
                budget,
                rate,
                expires_ms,
                tags: rk.tags.clone(),
                enabled: rk.enabled,
                created_ms: 0,
            });
        }
    }
    defs
}

fn validate_console(
    raw: &RawConfig,
    env: &dyn EnvSource,
    errors: &mut Vec<ConfigError>,
) -> super::ConsoleConfig {
    let c = &raw.console;
    let mut admin_keys = Vec::new();
    for (i, value) in c.admin_keys.iter().enumerate() {
        match resolve_secret(value, env) {
            Ok(secret) => admin_keys.push(secret),
            Err(msg) => errors.push(ConfigError::new(format!("console.admin_keys[{i}]"), msg)),
        }
    }
    if !(60..=604_800).contains(&c.session_ttl_secs) {
        errors.push(ConfigError::new(
            "console.session_ttl_secs",
            "must be between 60 (1 minute) and 604800 (7 days)",
        ));
    }
    super::ConsoleConfig {
        admin_keys,
        session_ttl: Duration::from_secs(c.session_ttl_secs),
    }
}

fn validate_store(raw: &RawConfig, errors: &mut Vec<ConfigError>) -> super::StoreConfig {
    let s = &raw.store;
    match s.backend.as_str() {
        "file" | "memory" => {}
        "s3" => {
            if s.bucket.as_deref().unwrap_or("").is_empty() {
                errors.push(ConfigError::new(
                    "store.bucket",
                    "required when `store.backend` is `s3`",
                ));
            }
        }
        "dynamodb" => {
            if s.table.as_deref().unwrap_or("").is_empty() {
                errors.push(ConfigError::new(
                    "store.table",
                    "required when `store.backend` is `dynamodb`",
                ));
            }
        }
        other => errors.push(ConfigError::new(
            "store.backend",
            format!("unknown backend `{other}` — expected `file`, `s3`, or `dynamodb`"),
        )),
    }
    // A liveness window shorter than the heartbeat means every node ages
    // itself out between beats and the fleet count oscillates.
    if s.liveness_window_secs <= s.heartbeat_interval_secs {
        errors.push(ConfigError::new(
            "store.liveness_window_secs",
            format!(
                "must be greater than store.heartbeat_interval_secs ({}s), or nodes age out \
                 between their own heartbeats",
                s.heartbeat_interval_secs
            ),
        ));
    }
    if s.refresh_interval_secs == 0 || s.heartbeat_interval_secs == 0 {
        errors.push(ConfigError::new(
            "store.refresh_interval_secs",
            "intervals must be at least 1 second",
        ));
    }
    super::StoreConfig {
        backend: s.backend.clone(),
        bucket: s.bucket.clone(),
        prefix: s.prefix.clone(),
        table: s.table.clone(),
        region: s.region.clone(),
        endpoint: s.endpoint.clone(),
        refresh_interval_secs: s.refresh_interval_secs,
        heartbeat_interval_secs: s.heartbeat_interval_secs,
        liveness_window_secs: s.liveness_window_secs,
    }
}

fn validate_usage(raw: &RawConfig, errors: &mut Vec<ConfigError>) -> super::UsageConfig {
    let u = &raw.usage;
    if !(1..=3650).contains(&u.retention_days) {
        errors.push(ConfigError::new(
            "usage.retention_days",
            "must be between 1 and 3650",
        ));
    }
    if !(1..=300).contains(&u.flush_interval_secs) {
        errors.push(ConfigError::new(
            "usage.flush_interval_secs",
            "must be between 1 and 300",
        ));
    }
    super::UsageConfig {
        retention_days: u.retention_days,
        flush_interval: Duration::from_secs(u.flush_interval_secs),
        per_key_metrics: u.per_key_metrics,
    }
}

fn validate_pricing(
    raw: &RawConfig,
    errors: &mut Vec<ConfigError>,
) -> BTreeMap<String, super::Price> {
    let mut pricing = BTreeMap::new();
    for (model, p) in &raw.pricing {
        let ok = p.input_per_mtok.is_finite()
            && p.input_per_mtok >= 0.0
            && p.output_per_mtok.is_finite()
            && p.output_per_mtok >= 0.0;
        if !ok {
            errors.push(ConfigError::new(
                format!("pricing.{model}"),
                "prices must be non-negative numbers (USD per million tokens)",
            ));
            continue;
        }
        pricing.insert(
            model.clone(),
            super::Price {
                input_per_mtok: p.input_per_mtok,
                output_per_mtok: p.output_per_mtok,
            },
        );
    }
    pricing
}

fn validate_providers(
    raw: &RawConfig,
    env: &dyn EnvSource,
    errors: &mut Vec<ConfigError>,
) -> BTreeMap<String, Provider> {
    let mut providers = BTreeMap::new();
    for (name, rp) in &raw.providers {
        let path = format!("providers.{name}");
        if let Some(provider) = validate_provider(name, rp, &path, env, errors) {
            providers.insert(name.clone(), provider);
        }
    }
    providers
}

fn validate_provider(
    name: &str,
    rp: &RawProvider,
    path: &str,
    env: &dyn EnvSource,
    errors: &mut Vec<ConfigError>,
) -> Option<Provider> {
    let before = errors.len();

    // Resolve the adapter kind: well-known name, or explicit type.
    let known = preset(name);
    let kind = match (&known, rp.r#type.as_deref()) {
        (Some(p), None) => p.kind,
        (None, Some("openai_compat")) => ProviderKind::OpenAiCompat,
        (_, Some("claude_subscription")) => ProviderKind::ClaudeSubscription,
        (_, Some("codex_subscription")) => ProviderKind::CodexSubscription,
        (None, Some(other)) => {
            errors.push(ConfigError::new(
                format!("{path}.type"),
                format!(
                    "unknown provider type `{other}` (expected `openai_compat`, \
                     `claude_subscription`, or `codex_subscription`)"
                ),
            ));
            return None;
        }
        (None, None) => {
            errors.push(ConfigError::new(
                path.to_owned(),
                format!(
                    "`{name}` is not a well-known provider; set `type = \"openai_compat\"` and a `base_url`"
                ),
            ));
            return None;
        }
        (Some(p), Some(t)) => {
            if !(p.kind == ProviderKind::OpenAiCompat && t == "openai_compat") {
                errors.push(ConfigError::new(
                    format!("{path}.type"),
                    format!("`{name}` is well-known; `type` must be omitted"),
                ));
            }
            p.kind
        }
    };

    let base_url = rp
        .base_url
        .clone()
        .or_else(|| known.as_ref().and_then(|p| p.base_url.map(str::to_owned)));
    if kind == ProviderKind::OpenAiCompat && base_url.is_none() {
        errors.push(ConfigError::new(
            format!("{path}.base_url"),
            "required for openai_compat providers",
        ));
    }
    if let Some(url) = &base_url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        errors.push(ConfigError::new(
            format!("{path}.base_url"),
            "must start with http:// or https://",
        ));
    }

    let auth = match rp.auth.as_deref() {
        None => {
            let keyless = known.as_ref().is_some_and(|p| p.keyless_ok);
            if keyless && rp.keys.is_empty() {
                AuthMode::None
            } else {
                AuthMode::Key
            }
        }
        Some("none") => AuthMode::None,
        Some("bearer") | Some("key") => AuthMode::Key,
        Some(other) => {
            errors.push(ConfigError::new(
                format!("{path}.auth"),
                format!("unknown auth mode `{other}` (expected `bearer` or `none`)"),
            ));
            AuthMode::Key
        }
    };

    if auth == AuthMode::Key && rp.keys.is_empty() {
        errors.push(ConfigError::new(
            format!("{path}.keys"),
            "at least one key is required (or set `auth = \"none\"` for keyless servers)",
        ));
    }

    let mut key_names = BTreeSet::new();
    let mut keys = Vec::new();
    for (i, rk) in rp.keys.iter().enumerate() {
        let kpath = format!("{path}.keys[{i}]");
        if rk.name.is_empty() {
            errors.push(ConfigError::new(
                format!("{kpath}.name"),
                "must not be empty",
            ));
        } else if !key_names.insert(rk.name.clone()) {
            errors.push(ConfigError::new(
                format!("{kpath}.name"),
                format!("duplicate key name `{}`", rk.name),
            ));
        }
        if !(rk.weight.is_finite() && rk.weight > 0.0) {
            errors.push(ConfigError::new(format!("{kpath}.weight"), "must be > 0"));
        }
        for (field, limit) in [("rpm", rk.rpm), ("tpm", rk.tpm)] {
            if limit == Some(0) {
                errors.push(ConfigError::new(
                    format!("{kpath}.{field}"),
                    "must be > 0 — omit the field to leave this key unlimited",
                ));
            }
        }
        match resolve_secret(&rk.value, env) {
            Ok(secret) => keys.push(ApiKey {
                name: rk.name.clone(),
                secret,
                weight: rk.weight,
                models: rk.models.clone(),
                rpm: rk.rpm,
                tpm: rk.tpm,
                source_path: rk
                    .value
                    .strip_prefix("file:")
                    .filter(|p| !p.is_empty())
                    .map(str::to_owned),
            }),
            Err(msg) => errors.push(ConfigError::new(format!("{kpath}.value"), msg)),
        }
    }

    if rp.max_concurrency == 0 {
        errors.push(ConfigError::new(
            format!("{path}.max_concurrency"),
            "must be > 0",
        ));
    }
    if rp.timeout_secs == 0 {
        errors.push(ConfigError::new(
            format!("{path}.timeout_secs"),
            "must be > 0",
        ));
    }

    let azure = validate_azure(kind, rp, path, errors);
    for field in ["endpoint", "api_version"] {
        let set = match field {
            "endpoint" => rp.endpoint.is_some(),
            _ => rp.api_version.is_some(),
        };
        if set && kind != ProviderKind::Azure {
            errors.push(ConfigError::new(
                format!("{path}.{field}"),
                "only valid for the azure provider",
            ));
        }
    }
    if !rp.deployments.is_empty() && kind != ProviderKind::Azure {
        errors.push(ConfigError::new(
            format!("{path}.deployments"),
            "only valid for the azure provider",
        ));
    }

    let bedrock = validate_bedrock(kind, rp, path, env, errors);
    let vertex = validate_vertex(kind, rp, path, errors);
    for field in ["project", "location"] {
        let set = match field {
            "project" => rp.project.is_some(),
            _ => rp.location.is_some(),
        };
        if set && kind != ProviderKind::Vertex {
            errors.push(ConfigError::new(
                format!("{path}.{field}"),
                "only valid for the vertex provider",
            ));
        }
    }
    for field in ["region", "access_key_id"] {
        let set = match field {
            "region" => rp.region.is_some(),
            _ => rp.access_key_id.is_some(),
        };
        if set && kind != ProviderKind::Bedrock {
            errors.push(ConfigError::new(
                format!("{path}.{field}"),
                "only valid for the bedrock provider",
            ));
        }
    }

    let codex = validate_codex(kind, rp, path, errors);

    // Azure's endpoint doubles as its base URL; Bedrock defaults to its
    // regional runtime endpoint unless overridden. The subscription
    // transports have one endpoint each and no reason to be pointed
    // elsewhere in normal use, but stay overridable for testing.
    let base_url = match (kind, &azure, &bedrock, &vertex) {
        (ProviderKind::Azure, Some(a), _, _) => Some(a.endpoint.clone()),
        (ProviderKind::Bedrock, _, Some(b), _) => base_url.or_else(|| {
            Some(format!(
                "https://bedrock-runtime.{}.amazonaws.com",
                b.region
            ))
        }),
        (ProviderKind::Vertex, _, _, Some(v)) => base_url.or_else(|| {
            Some(if v.location == "global" {
                "https://aiplatform.googleapis.com".to_owned()
            } else {
                format!("https://{}-aiplatform.googleapis.com", v.location)
            })
        }),
        (ProviderKind::ClaudeSubscription, ..) => {
            base_url.or_else(|| Some("https://api.anthropic.com".to_owned()))
        }
        (ProviderKind::CodexSubscription, ..) => {
            base_url.or_else(|| Some("https://chatgpt.com".to_owned()))
        }
        _ => base_url,
    };

    if errors.len() > before {
        return None;
    }
    Some(Provider {
        kind,
        base_url,
        auth,
        keys,
        max_concurrency: rp.max_concurrency,
        timeout: Duration::from_secs(rp.timeout_secs),
        azure,
        bedrock,
        vertex,
        codex,
    })
}

/// Validate the `[providers.x.codex]` block.
///
/// Both enum knobs accept `""` — "send no floor, take the backend's own
/// default" — which is why they are `Option<String>` rather than a
/// defaulted string. An out-of-vocabulary value is rejected here rather
/// than forwarded, because the backend answers one with a 400 on *every*
/// request: a typo in a config file would take the whole provider down,
/// and a config that cannot serve traffic must not validate.
fn validate_codex(
    kind: ProviderKind,
    rp: &RawProvider,
    path: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<CodexSettings> {
    const EFFORTS: [&str; 7] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];
    const VERBOSITIES: [&str; 3] = ["low", "medium", "high"];

    if kind != ProviderKind::CodexSubscription {
        if rp.codex.is_some() {
            errors.push(ConfigError::new(
                format!("{path}.codex"),
                "only valid for `type = \"codex_subscription\"` providers",
            ));
        }
        return None;
    }

    let raw = rp.codex.as_ref();
    let mut settings = CodexSettings::default();
    if let Some(version) = raw.and_then(|c| c.version.as_deref()) {
        if version.is_empty() {
            errors.push(ConfigError::new(
                format!("{path}.codex.version"),
                "must not be empty — the backend gates models on this value",
            ));
        } else {
            settings.version = version.to_owned();
        }
    }
    let mut choice =
        |value: Option<&str>, allowed: &[&str], field: &str| -> Option<Option<String>> {
            let value = value?;
            if value.is_empty() {
                return Some(None);
            }
            if !allowed.contains(&value) {
                errors.push(ConfigError::new(
                    format!("{path}.codex.{field}"),
                    format!("must be one of {allowed:?}, or \"\" for the provider default"),
                ));
                return None;
            }
            Some(Some(value.to_owned()))
        };
    if let Some(effort) = choice(
        raw.and_then(|c| c.reasoning_effort.as_deref()),
        &EFFORTS,
        "reasoning_effort",
    ) {
        settings.reasoning_effort = effort;
    }
    if let Some(verbosity) = choice(
        raw.and_then(|c| c.verbosity.as_deref()),
        &VERBOSITIES,
        "verbosity",
    ) {
        settings.verbosity = verbosity;
    }
    Some(settings)
}

fn validate_azure(
    kind: ProviderKind,
    rp: &RawProvider,
    path: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<AzureSettings> {
    if kind != ProviderKind::Azure {
        return None;
    }
    let endpoint = match &rp.endpoint {
        Some(e) if e.starts_with("https://") || e.starts_with("http://") => Some(e.clone()),
        Some(_) => {
            errors.push(ConfigError::new(
                format!("{path}.endpoint"),
                "must start with http:// or https://",
            ));
            None
        }
        None => {
            errors.push(ConfigError::new(
                format!("{path}.endpoint"),
                "required for azure",
            ));
            None
        }
    };
    let api_version = match &rp.api_version {
        Some(v) => Some(v.clone()),
        None => {
            errors.push(ConfigError::new(
                format!("{path}.api_version"),
                "required for azure",
            ));
            None
        }
    };
    Some(AzureSettings {
        endpoint: endpoint?,
        api_version: api_version?,
        deployments: rp.deployments.clone(),
    })
}

fn validate_bedrock(
    kind: ProviderKind,
    rp: &RawProvider,
    path: &str,
    env: &dyn EnvSource,
    errors: &mut Vec<ConfigError>,
) -> Option<BedrockSettings> {
    if kind != ProviderKind::Bedrock {
        return None;
    }
    let region = match &rp.region {
        Some(r) if !r.is_empty() => Some(r.clone()),
        _ => {
            errors.push(ConfigError::new(
                format!("{path}.region"),
                "required for bedrock",
            ));
            None
        }
    };
    let access_key_id = match &rp.access_key_id {
        Some(reference) => match resolve_secret(reference, env) {
            Ok(secret) => Some(secret.expose().to_owned()),
            Err(msg) => {
                errors.push(ConfigError::new(format!("{path}.access_key_id"), msg));
                None
            }
        },
        None => {
            errors.push(ConfigError::new(
                format!("{path}.access_key_id"),
                "required for bedrock (the key's `value` is the secret access key)",
            ));
            None
        }
    };
    Some(BedrockSettings {
        region: region?,
        access_key_id: access_key_id?,
    })
}

fn validate_vertex(
    kind: ProviderKind,
    rp: &RawProvider,
    path: &str,
    errors: &mut Vec<ConfigError>,
) -> Option<VertexSettings> {
    if kind != ProviderKind::Vertex {
        return None;
    }
    let project = match &rp.project {
        Some(p) if !p.is_empty() => Some(p.clone()),
        _ => {
            errors.push(ConfigError::new(
                format!("{path}.project"),
                "required for vertex",
            ));
            None
        }
    };
    Some(VertexSettings {
        project: project?,
        location: rp
            .location
            .clone()
            .unwrap_or_else(|| "us-central1".to_owned()),
    })
}

fn validate_aliases(
    raw: &RawConfig,
    providers: &BTreeMap<String, Provider>,
    errors: &mut Vec<ConfigError>,
) -> BTreeMap<String, TargetModel> {
    let mut resolved = BTreeMap::new();
    for name in raw.aliases.keys() {
        let path = format!("aliases.{name}");
        if raw.providers.contains_key(name) {
            errors.push(ConfigError::new(
                path,
                "alias name collides with a provider name",
            ));
            continue;
        }
        match resolve_alias(name, &raw.aliases, &mut BTreeSet::new()) {
            Ok(target_str) => match TargetModel::parse(&target_str) {
                Some(target) => {
                    if providers.contains_key(&target.provider)
                        || raw.providers.contains_key(&target.provider)
                    {
                        resolved.insert(name.clone(), target);
                    } else {
                        errors.push(ConfigError::new(
                            format!("aliases.{name}"),
                            format!(
                                "unknown provider `{}` in target `{target_str}`",
                                target.provider
                            ),
                        ));
                    }
                }
                None => errors.push(ConfigError::new(
                    format!("aliases.{name}"),
                    format!("target `{target_str}` is not `provider/model` or another alias"),
                )),
            },
            Err(msg) => errors.push(ConfigError::new(format!("aliases.{name}"), msg)),
        }
    }
    resolved
}

/// Follow alias -> alias chains to a terminal `provider/model` string,
/// detecting cycles.
fn resolve_alias(
    name: &str,
    aliases: &BTreeMap<String, String>,
    seen: &mut BTreeSet<String>,
) -> Result<String, String> {
    if !seen.insert(name.to_owned()) {
        return Err(format!("alias cycle involving `{name}`"));
    }
    let target = aliases
        .get(name)
        .expect("caller only passes existing aliases");
    if aliases.contains_key(target) {
        return resolve_alias(target, aliases, seen);
    }
    Ok(target.clone())
}

fn validate_fallbacks(
    raw: &RawConfig,
    providers: &BTreeMap<String, Provider>,
    aliases: &BTreeMap<String, TargetModel>,
    errors: &mut Vec<ConfigError>,
) -> BTreeMap<TargetModel, Vec<TargetModel>> {
    let mut resolved = BTreeMap::new();
    let resolve = |s: &str, path: String, errors: &mut Vec<ConfigError>| -> Option<TargetModel> {
        if let Some(target) = aliases.get(s) {
            return Some(target.clone());
        }
        match TargetModel::parse(s) {
            Some(t)
                if providers.contains_key(&t.provider)
                    || raw.providers.contains_key(&t.provider) =>
            {
                Some(t)
            }
            Some(t) => {
                errors.push(ConfigError::new(
                    path,
                    format!("unknown provider `{}`", t.provider),
                ));
                None
            }
            None => {
                errors.push(ConfigError::new(
                    path,
                    format!("`{s}` is not `provider/model` or a defined alias"),
                ));
                None
            }
        }
    };

    for (from, chain) in &raw.fallbacks {
        let Some(from_target) = resolve(from, format!("fallbacks.{from}"), errors) else {
            continue;
        };
        if chain.is_empty() {
            errors.push(ConfigError::new(
                format!("fallbacks.{from}"),
                "fallback chain must not be empty",
            ));
            continue;
        }
        let mut targets = Vec::new();
        for (i, entry) in chain.iter().enumerate() {
            if let Some(t) = resolve(entry, format!("fallbacks.{from}[{i}]"), errors) {
                if t == from_target {
                    errors.push(ConfigError::new(
                        format!("fallbacks.{from}[{i}]"),
                        "fallback target equals its own source",
                    ));
                } else {
                    targets.push(t);
                }
            }
        }
        resolved.insert(from_target, targets);
    }
    resolved
}

fn validate_reliability(raw: &RawConfig, errors: &mut Vec<ConfigError>) -> Reliability {
    let r = &raw.reliability;
    if r.breaker.failure_threshold == 0 {
        errors.push(ConfigError::new(
            "reliability.breaker.failure_threshold",
            "must be > 0",
        ));
    }
    let mut on = Vec::new();
    for (i, s) in r.retries.on.iter().enumerate() {
        match s.as_str() {
            "connect_error" => on.push(RetryOn::ConnectError),
            "429" => on.push(RetryOn::Status429),
            "5xx" => on.push(RetryOn::Status5xx),
            other => errors.push(ConfigError::new(
                format!("reliability.retries.on[{i}]"),
                format!("unknown retry condition `{other}` (expected connect_error, 429, 5xx)"),
            )),
        }
    }
    Reliability {
        breaker: Breaker {
            failure_threshold: r.breaker.failure_threshold,
            window: Duration::from_secs(r.breaker.window_secs),
            cooldown: Duration::from_secs(r.breaker.cooldown_secs),
        },
        retries: Retries {
            max_attempts: r.retries.max_attempts,
            on,
        },
    }
}

/// Resolve a config secret reference to a `SecretString`.
///
/// `env.VAR` reads the environment; `store.name` is reserved for the
/// managed store; anything else is taken as a literal value.
fn resolve_secret(value: &str, env: &dyn EnvSource) -> Result<SecretString, String> {
    if let Some(path) = value.strip_prefix("file:") {
        // A whole credential document read from disk: a Codex `auth.json`
        // or a Claude Code credential file, which are what a subscription
        // seat is configured with. Trailing whitespace is stripped so a
        // file ending in a newline — every file an editor writes — still
        // yields a usable token.
        if path.is_empty() {
            return Err("empty path after `file:`".into());
        }
        return match std::fs::read_to_string(path) {
            Ok(contents) if !contents.trim().is_empty() => {
                Ok(SecretString::new(contents.trim().to_owned()))
            }
            Ok(_) => Err(format!("credential file `{path}` is empty")),
            Err(e) => Err(format!("cannot read credential file `{path}`: {e}")),
        };
    }
    if let Some(var) = value.strip_prefix("env.") {
        if var.is_empty() {
            return Err("empty environment variable name after `env.`".into());
        }
        match env.get(var) {
            Some(v) if !v.is_empty() => Ok(SecretString::new(v)),
            Some(_) => Err(format!("environment variable `{var}` is set but empty")),
            None => Err(format!("environment variable `{var}` is not set")),
        }
    } else if let Some(name) = value.strip_prefix("store.") {
        if name.is_empty() {
            return Err("empty secret name after `store.`".into());
        }
        // Managed mode passes a source that also answers `store.<name>`
        // lookups from the sealed secret store; a plain environment
        // source answers None.
        match env.get(value) {
            Some(v) if !v.is_empty() => Ok(SecretString::new(v)),
            _ => Err(format!(
                "store secret `{name}` is not set (managed mode: `rapid-router secret set {name}`)"
            )),
        }
    } else if value.is_empty() {
        Err("must not be empty".into())
    } else {
        Ok(SecretString::new(value.to_owned()))
    }
}
