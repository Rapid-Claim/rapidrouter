//! Total validation of a raw config document into the resolved [`Config`].
//! Collects every error with its document path rather than failing fast.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use super::presets::preset;
use super::raw::{RawConfig, RawProvider};
use super::{
    ApiKey, AuthMode, AzureSettings, Breaker, Config, ConfigError, Provider, ProviderKind,
    Reliability, Retries, RetryOn, ServerConfig, TargetModel,
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

    if errors.is_empty() {
        Ok(Config {
            server,
            providers,
            aliases,
            fallbacks,
            reliability,
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
        drain_timeout: Duration::from_secs(s.drain_timeout_secs),
    }
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
        (None, Some(other)) => {
            errors.push(ConfigError::new(
                format!("{path}.type"),
                format!("unknown provider type `{other}` (expected `openai_compat`)"),
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
        match resolve_secret(&rk.value, env) {
            Ok(secret) => keys.push(ApiKey {
                name: rk.name.clone(),
                secret,
                weight: rk.weight,
                models: rk.models.clone(),
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
    })
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
        Some(e) if e.starts_with("https://") => Some(e.clone()),
        Some(_) => {
            errors.push(ConfigError::new(
                format!("{path}.endpoint"),
                "must start with https://",
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
    if let Some(var) = value.strip_prefix("env.") {
        if var.is_empty() {
            return Err("empty environment variable name after `env.`".into());
        }
        match env.get(var) {
            Some(v) if !v.is_empty() => Ok(SecretString::new(v)),
            Some(_) => Err(format!("environment variable `{var}` is set but empty")),
            None => Err(format!("environment variable `{var}` is not set")),
        }
    } else if value.strip_prefix("store.").is_some() {
        Err("`store.*` secrets require managed mode, which is not available yet".into())
    } else if value.is_empty() {
        Err("must not be empty".into())
    } else {
        Ok(SecretString::new(value.to_owned()))
    }
}
