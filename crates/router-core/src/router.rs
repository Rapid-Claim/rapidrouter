//! Model resolution, health-aware key selection, and fallback planning
//! over an immutable routing snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::breaker::{Admission, Breaker, BreakerConfig};
use crate::config::{AuthMode, Config, ProviderKind, Retries, RetryOn, TargetModel};
use crate::error::{ErrorClass, GatewayError};
use crate::secret::SecretString;

pub struct RoutingTable {
    providers: BTreeMap<String, Arc<ProviderRuntime>>,
    /// Bare model name -> provider name, from key allowlists.
    catalog: BTreeMap<String, String>,
    aliases: BTreeMap<String, TargetModel>,
    fallbacks: BTreeMap<TargetModel, Vec<TargetModel>>,
    retries: Retries,
}

#[derive(Debug)]
pub struct ProviderRuntime {
    pub name: String,
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    pub auth: AuthMode,
    pub keys: Vec<KeyRuntime>,
    pub timeout: Duration,
    pub semaphore: Arc<Semaphore>,
    /// Health for keyless providers (local servers), where there is no
    /// key to hang a breaker on.
    pub provider_breaker: Breaker,
}

#[derive(Debug)]
pub struct KeyRuntime {
    pub name: String,
    pub secret: SecretString,
    pub weight: f64,
    /// `None` = serves every model of this provider.
    pub models: Option<BTreeSet<String>>,
    pub breaker: Breaker,
}

#[derive(Clone, Debug)]
pub struct ResolvedRoute {
    pub provider: Arc<ProviderRuntime>,
    /// The model name as the upstream expects it (prefix stripped, alias
    /// resolved).
    pub upstream_model: String,
}

/// Everything the dispatch loop needs: the ordered candidate targets and
/// the retry policy in force.
pub struct RoutePlan {
    pub targets: Vec<ResolvedRoute>,
    pub max_attempts_per_target: u32,
    pub retry_on: Vec<RetryOn>,
}

/// A key admitted for one attempt. `key` is `None` for keyless providers;
/// `admission` says whether this was a healthy pick or a breaker probe.
pub struct KeyChoice<'a> {
    pub key: Option<&'a KeyRuntime>,
    pub admission: Admission,
}

impl RoutingTable {
    pub fn from_config(config: &Config) -> Self {
        let breaker_config = BreakerConfig {
            failure_threshold: config.reliability.breaker.failure_threshold,
            window_ms: config.reliability.breaker.window.as_millis() as u64,
            cooldown_ms: config.reliability.breaker.cooldown.as_millis() as u64,
        };

        let mut providers = BTreeMap::new();
        let mut catalog = BTreeMap::new();
        for (name, p) in &config.providers {
            let keys: Vec<KeyRuntime> = p
                .keys
                .iter()
                .map(|k| KeyRuntime {
                    name: k.name.clone(),
                    secret: k.secret.clone(),
                    weight: k.weight,
                    models: k.models.as_ref().map(|m| m.iter().cloned().collect()),
                    breaker: Breaker::new(breaker_config),
                })
                .collect();
            for key in &keys {
                for model in key.models.iter().flatten() {
                    catalog.entry(model.clone()).or_insert_with(|| name.clone());
                }
            }
            providers.insert(
                name.clone(),
                Arc::new(ProviderRuntime {
                    name: name.clone(),
                    kind: p.kind,
                    base_url: p.base_url.clone(),
                    auth: p.auth,
                    keys,
                    timeout: p.timeout,
                    semaphore: Arc::new(Semaphore::new(p.max_concurrency)),
                    provider_breaker: Breaker::new(breaker_config),
                }),
            );
        }

        Self {
            providers,
            catalog,
            aliases: config.aliases.clone(),
            fallbacks: config.fallbacks.clone(),
            retries: config.retries().clone(),
        }
    }

    /// Resolve a requested model string: alias, `provider/model` prefix,
    /// or bare catalog name — in that order.
    pub fn resolve(&self, requested: &str) -> Result<ResolvedRoute, GatewayError> {
        let target = self.resolve_target(requested)?;
        self.route_to(&target, requested)
    }

    /// The full dispatch plan: primary target plus its configured
    /// fallback chain (skipping any fallback whose provider vanished in
    /// a reload), and the retry policy.
    pub fn plan(&self, requested: &str) -> Result<RoutePlan, GatewayError> {
        let primary = self.resolve_target(requested)?;
        let mut targets = vec![self.route_to(&primary, requested)?];
        if let Some(chain) = self.fallbacks.get(&primary) {
            for target in chain {
                if let Ok(route) = self.route_to(target, requested) {
                    targets.push(route);
                }
            }
        }
        Ok(RoutePlan {
            targets,
            max_attempts_per_target: self.retries.max_attempts.max(1),
            retry_on: self.retries.on.clone(),
        })
    }

    fn resolve_target(&self, requested: &str) -> Result<TargetModel, GatewayError> {
        if let Some(target) = self.aliases.get(requested) {
            return Ok(target.clone());
        }
        if let Some(target) = TargetModel::parse(requested)
            && self.providers.contains_key(&target.provider)
        {
            return Ok(target);
        }
        if let Some(provider) = self.catalog.get(requested) {
            return Ok(TargetModel {
                provider: provider.clone(),
                model: requested.to_owned(),
            });
        }
        Err(GatewayError::new(
            ErrorClass::NotFound,
            format!(
                "unknown model `{requested}`; use `provider/model`, a configured alias, \
                 or a model listed in a key's `models`"
            ),
        )
        .with_param("model"))
    }

    fn route_to(
        &self,
        target: &TargetModel,
        requested: &str,
    ) -> Result<ResolvedRoute, GatewayError> {
        let runtime = self.providers.get(&target.provider).ok_or_else(|| {
            GatewayError::new(
                ErrorClass::NotFound,
                format!(
                    "model `{requested}` routes to unconfigured provider `{}`",
                    target.provider
                ),
            )
            .with_param("model")
        })?;
        Ok(ResolvedRoute {
            provider: runtime.clone(),
            upstream_model: target.model.clone(),
        })
    }

    pub fn providers(&self) -> impl Iterator<Item = &Arc<ProviderRuntime>> {
        self.providers.values()
    }

    pub fn aliases(&self) -> &BTreeMap<String, TargetModel> {
        &self.aliases
    }

    pub fn catalog(&self) -> &BTreeMap<String, String> {
        &self.catalog
    }
}

impl ProviderRuntime {
    /// Admit one attempt for `model`: weighted-random among healthy
    /// eligible keys; if none is healthy, offer the probe slot of an
    /// unhealthy one (single racer wins, per breaker semantics).
    ///
    /// `None` means nothing is admitted right now: every eligible key is
    /// open and in cooldown (or no key serves this model at all).
    pub fn admit_key(&self, model: &str, now_ms: u64) -> Option<KeyChoice<'_>> {
        if self.keys.is_empty() {
            return match self.provider_breaker.admit(now_ms) {
                Admission::No => None,
                admission => Some(KeyChoice {
                    key: None,
                    admission,
                }),
            };
        }

        let eligible: Vec<&KeyRuntime> = self
            .keys
            .iter()
            .filter(|k| k.models.as_ref().is_none_or(|m| m.contains(model)))
            .collect();
        if eligible.is_empty() {
            return None;
        }

        let healthy: Vec<&KeyRuntime> = eligible
            .iter()
            .copied()
            .filter(|k| k.breaker.looks_healthy())
            .collect();
        if !healthy.is_empty() {
            let picked = weighted_pick(&healthy);
            return Some(KeyChoice {
                key: Some(picked),
                admission: Admission::Yes,
            });
        }

        for key in eligible {
            if key.breaker.admit(now_ms) == Admission::Probe {
                return Some(KeyChoice {
                    key: Some(key),
                    admission: Admission::Probe,
                });
            }
        }
        None
    }

    /// The breaker an attempt outcome should be recorded against.
    pub fn breaker_for<'a>(&'a self, key: Option<&'a KeyRuntime>) -> &'a Breaker {
        key.map(|k| &k.breaker).unwrap_or(&self.provider_breaker)
    }

    /// Health-unaware selection, for surfaces that only need a key (e.g.
    /// tests, catalog listings).
    pub fn select_key(&self, model: &str) -> Option<&KeyRuntime> {
        let eligible: Vec<&KeyRuntime> = self
            .keys
            .iter()
            .filter(|k| k.models.as_ref().is_none_or(|m| m.contains(model)))
            .collect();
        if eligible.is_empty() {
            None
        } else {
            Some(weighted_pick(&eligible))
        }
    }
}

fn weighted_pick<'a>(keys: &[&'a KeyRuntime]) -> &'a KeyRuntime {
    match keys.len() {
        1 => keys[0],
        _ => {
            let total: f64 = keys.iter().map(|k| k.weight).sum();
            let mut roll = fastrand::f64() * total;
            for key in keys {
                roll -= key.weight;
                if roll <= 0.0 {
                    return key;
                }
            }
            keys.last().expect("non-empty by construction")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Format;

    fn table(toml: &str) -> RoutingTable {
        let config = Config::from_str_with_env(toml, Format::Toml, &|_: &str| None).unwrap();
        RoutingTable::from_config(&config)
    }

    const BASE: &str = r#"
[providers.openai]
keys = [
  { name = "a", value = "sk-a", weight = 0.5, models = ["gpt-4o"] },
  { name = "b", value = "sk-b", weight = 0.5 },
]

[providers.groq]
keys = [{ name = "main", value = "gsk", models = ["llama-3.3-70b"] }]

[aliases]
fast = "groq/llama-3.3-70b"

[fallbacks]
"openai/gpt-4o" = ["groq/llama-3.3-70b"]
"#;

    #[test]
    fn resolves_prefix_alias_and_catalog() {
        let t = table(BASE);
        assert_eq!(
            t.resolve("openai/gpt-4o-mini").unwrap().provider.name,
            "openai"
        );
        assert_eq!(t.resolve("fast").unwrap().upstream_model, "llama-3.3-70b");
        assert_eq!(t.resolve("llama-3.3-70b").unwrap().provider.name, "groq");
        assert_eq!(t.resolve("nope").unwrap_err().class, ErrorClass::NotFound);
    }

    #[test]
    fn plan_includes_fallback_chain() {
        let t = table(BASE);
        let plan = t.plan("openai/gpt-4o").unwrap();
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(plan.targets[0].provider.name, "openai");
        assert_eq!(plan.targets[1].provider.name, "groq");
        assert_eq!(plan.targets[1].upstream_model, "llama-3.3-70b");
        // No chain configured for this one.
        assert_eq!(t.plan("fast").unwrap().targets.len(), 1);
    }

    #[test]
    fn alias_beats_catalog_name() {
        let t = table(
            r#"
[providers.openai]
keys = [{ name = "a", value = "sk", models = ["fast"] }]
[providers.groq]
keys = [{ name = "m", value = "gsk" }]
[aliases]
fast = "groq/llama-3.3-70b"
"#,
        );
        assert_eq!(t.resolve("fast").unwrap().provider.name, "groq");
    }

    #[test]
    fn key_allowlists_filter_selection() {
        let t = table(BASE);
        let r = t.resolve("openai/other-model").unwrap();
        for _ in 0..50 {
            assert_eq!(r.provider.select_key("other-model").unwrap().name, "b");
        }
    }

    #[test]
    fn weighted_selection_roughly_follows_weights() {
        let t = table(
            r#"
[providers.openai]
keys = [
  { name = "heavy", value = "sk-a", weight = 0.9 },
  { name = "light", value = "sk-b", weight = 0.1 },
]
"#,
        );
        fastrand::seed(7);
        let r = t.resolve("openai/gpt-4o").unwrap();
        let heavy = (0..2000)
            .filter(|_| r.provider.select_key("gpt-4o").unwrap().name == "heavy")
            .count();
        assert!(
            (1600..=1990).contains(&heavy),
            "heavy selected {heavy}/2000"
        );
    }

    #[test]
    fn admit_skips_open_breakers_and_probes_after_cooldown() {
        let t = table(
            r#"
[providers.openai]
keys = [
  { name = "a", value = "sk-a" },
  { name = "b", value = "sk-b" },
]
[reliability.breaker]
failure_threshold = 2
window_secs = 30
cooldown_secs = 1
"#,
        );
        let r = t.resolve("openai/m").unwrap();

        // Open key `a`.
        let a = r.provider.keys.iter().find(|k| k.name == "a").unwrap();
        a.breaker.record_failure(0);
        a.breaker.record_failure(1);

        // While `a` is open, admission must always land on `b`.
        for _ in 0..50 {
            let choice = r.provider.admit_key("m", 10).unwrap();
            assert_eq!(choice.key.unwrap().name, "b");
            assert_eq!(choice.admission, Admission::Yes);
        }

        // Open `b` too: nothing healthy, nothing past cooldown -> None.
        let b = r.provider.keys.iter().find(|k| k.name == "b").unwrap();
        b.breaker.record_failure(2);
        b.breaker.record_failure(3);
        assert!(r.provider.admit_key("m", 10).is_none());

        // Past cooldown: one probe per open key, then nothing.
        let first = r.provider.admit_key("m", 1500).unwrap();
        assert_eq!(first.admission, Admission::Probe);
        let second = r.provider.admit_key("m", 1500).unwrap();
        assert_eq!(second.admission, Admission::Probe);
        assert_ne!(first.key.unwrap().name, second.key.unwrap().name);
        assert!(r.provider.admit_key("m", 1500).is_none());
    }

    #[test]
    fn keyless_provider_uses_provider_breaker() {
        let t = table("[providers.ollama]\nauth = \"none\"\n");
        let r = t.resolve("ollama/llama3").unwrap();
        let choice = r.provider.admit_key("llama3", 0).unwrap();
        assert!(choice.key.is_none());
        r.provider.provider_breaker.record_failure(0);
        for _ in 0..4 {
            r.provider.provider_breaker.record_failure(0);
        }
        assert!(r.provider.admit_key("llama3", 1).is_none());
    }
}
