//! Model resolution, health-aware key selection, and fallback planning
//! over an immutable routing snapshot.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::Semaphore;

use crate::breaker::{Admission, Breaker, BreakerConfig};
use crate::config::{
    AuthMode, AzureSettings, BedrockSettings, CodexSettings, Config, ProviderKind, Retries,
    RetryOn, RoutingGroup, TargetModel, VertexSettings, WeightedTarget,
};
use crate::credential::{self, Credential, Seat};
use crate::error::{ErrorClass, GatewayError};
use crate::quota::Quota;
use crate::secret::SecretString;
use crate::token_bucket::TokenBucket;

pub struct RoutingTable {
    providers: BTreeMap<String, Arc<ProviderRuntime>>,
    /// Bare model name -> provider name, from key allowlists.
    catalog: BTreeMap<String, String>,
    aliases: BTreeMap<String, TargetModel>,
    fallbacks: BTreeMap<TargetModel, Vec<TargetModel>>,
    groups: BTreeMap<String, RoutingGroup>,
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
    pub azure: Option<AzureSettings>,
    pub bedrock: Option<BedrockSettings>,
    pub vertex: Option<VertexSettings>,
    pub codex: Option<CodexSettings>,
}

#[derive(Debug)]
pub struct KeyRuntime {
    pub name: String,
    /// What this key presents upstream. A metered key is a constant; a
    /// subscription seat's credential rotates underneath, which is why
    /// this is read through [`KeyRuntime::token`] per request rather than
    /// borrowed once.
    pub credential: Credential,
    pub weight: f64,
    /// `None` = serves every model of this provider.
    pub models: Option<BTreeSet<String>>,
    pub breaker: Breaker,
    /// Where a rotated credential is persisted; see
    /// [`crate::config::ApiKey::source_path`].
    pub source_path: Option<String>,
    /// Per-key rate limits, enforced before the request leaves.
    ///
    /// A metered key's ceiling is the provider's published rate limit for
    /// that account; a subscription seat's is its plan quota, which the
    /// provider reports rather than us configuring. Both are per *key*,
    /// not per provider: one exhausted key must not stop the pool.
    pub rpm: Option<TokenBucket>,
    pub tpm: Option<TokenBucket>,
    /// The last quota view this key's provider reported, for the console.
    ///
    /// Observability only — benching is decided at the moment a response
    /// arrives (see the proxy) and lives in `breaker`. This is the
    /// snapshot an operator looks at to answer "how close to the edge is
    /// this seat", which is otherwise invisible until traffic starts
    /// failing.
    quota: Mutex<Option<QuotaSnapshot>>,
}

/// A quota reading with the wall-clock time it was taken, so the console
/// can say "as of 40 seconds ago" rather than implying it is live.
#[derive(Debug, Clone, Copy)]
pub struct QuotaSnapshot {
    pub quota: Quota,
    pub observed_ms: u64,
}

impl KeyRuntime {
    /// The credential value for this request.
    ///
    /// Owned rather than borrowed: a seat may be renewed while the caller
    /// is still assembling the request, and a request must use one
    /// consistent token from first byte to last.
    /// Record the provider's latest quota view for this key.
    pub fn observe_quota(&self, quota: Quota, now_ms: u64) {
        if quota.is_empty() {
            return;
        }
        if let Ok(mut slot) = self.quota.lock() {
            *slot = Some(QuotaSnapshot {
                quota,
                observed_ms: now_ms,
            });
        }
    }

    /// The latest quota view, if this key has ever served a request.
    pub fn quota(&self) -> Option<QuotaSnapshot> {
        self.quota.lock().ok().and_then(|slot| *slot)
    }

    /// Spend one request against this key's own ceiling. `false` means
    /// the key is over its limit and selection should try the next one.
    ///
    /// Requests are pre-paid (one per request, known up front); tokens
    /// are post-paid, because the true cost is not known until the
    /// response. So the token limiter is only *checked* here — an
    /// exhausted balance holds the key out until it refills — and
    /// [`Self::debit_tokens`] settles it afterwards.
    pub fn try_admit_request(&self, now_ms: u64) -> bool {
        if let Some(tpm) = &self.tpm {
            // A zero-token take is how the bucket is made to credit
            // elapsed time without spending any of it.
            tpm.try_consume(0, now_ms);
            if tpm.available_tokens() == 0 {
                return false;
            }
        }
        match &self.rpm {
            Some(rpm) => rpm.try_consume(1, now_ms),
            None => true,
        }
    }

    /// Settle the token limiter against what the request actually used.
    pub fn debit_tokens(&self, tokens: u64, now_ms: u64) {
        if let Some(tpm) = &self.tpm {
            tpm.debit_saturating(tokens, now_ms);
        }
    }

    /// Remaining allowance, for the console. `None` = unlimited.
    pub fn rate_headroom(&self) -> (Option<u64>, Option<u64>) {
        (
            self.rpm.as_ref().map(TokenBucket::available_tokens),
            self.tpm.as_ref().map(TokenBucket::available_tokens),
        )
    }

    pub fn token(&self) -> SecretString {
        self.credential.token()
    }

    /// The seat behind this key, for the paths that must renew it or read
    /// its account id. `None` for every metered key.
    pub fn seat(&self) -> Option<&Arc<Seat>> {
        self.credential.seat()
    }
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
        Self::from_config_with(config, None)
    }

    /// Rebuild, carrying per-key runtime state across the swap.
    ///
    /// A config reload must not hand every key a fresh rate-limit
    /// allowance — that would make "edit the config" a way to bypass the
    /// ceiling, and a config that reloads often would never enforce one.
    /// The observed quota is carried for the same reason in reverse:
    /// blanking it would make the console claim it knows nothing about a
    /// seat that is, in fact, still exhausted.
    pub fn from_config_with(config: &Config, prev: Option<&Self>) -> Self {
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
                    credential: build_credential(p.kind, &k.secret),
                    source_path: k.source_path.clone(),
                    weight: k.weight,
                    models: k.models.as_ref().map(|m| m.iter().cloned().collect()),
                    breaker: Breaker::new(breaker_config),
                    // A minute's worth of capacity, refilled per second,
                    // so a burst is allowed but a sustained overrun is
                    // not — the same shape the virtual-key limiter uses.
                    rpm: carry_bucket(
                        previous_key(prev, name, &k.name).and_then(|p| p.rpm.as_ref()),
                        k.rpm,
                    ),
                    tpm: carry_bucket(
                        previous_key(prev, name, &k.name).and_then(|p| p.tpm.as_ref()),
                        k.tpm,
                    ),
                    quota: Mutex::new(
                        previous_key(prev, name, &k.name).and_then(KeyRuntime::quota),
                    ),
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
                    azure: p.azure.clone(),
                    bedrock: p.bedrock.clone(),
                    vertex: p.vertex.clone(),
                    codex: p.codex.clone(),
                }),
            );
        }

        Self {
            providers,
            catalog,
            aliases: config.aliases.clone(),
            fallbacks: config.fallbacks.clone(),
            groups: config.groups.clone(),
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
        if let Some(group) = self.groups.get(requested) {
            return self.plan_group(requested, group);
        }
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

    /// A group's plan: the primary pool drawn in weighted-random order,
    /// then the fallback pool drawn the same way.
    ///
    /// Only the *head* of each draw carries the traffic split — that is
    /// the target a healthy request goes to, and over many requests it is
    /// hit in proportion to its weight. The tail is the order the pool is
    /// exhausted in when that target fails, and weighting it too means a
    /// heavy model is also the preferred *second* choice, which is what an
    /// operator who wrote 90/10 expects.
    fn plan_group(&self, requested: &str, group: &RoutingGroup) -> Result<RoutePlan, GatewayError> {
        let mut targets = Vec::new();
        for target in weighted_order(&group.primary)
            .into_iter()
            .chain(weighted_order(&group.fallback))
        {
            if let Ok(route) = self.route_to(&target, requested) {
                targets.push(route);
            }
        }
        if targets.is_empty() {
            return Err(GatewayError::new(
                ErrorClass::NotFound,
                format!(
                    "routing group `{requested}` has no usable target; every model in it \
                     names a provider this gateway is not configured for"
                ),
            )
            .with_param("model"));
        }
        Ok(RoutePlan {
            targets,
            max_attempts_per_target: self.retries.max_attempts.max(1),
            retry_on: self.retries.on.clone(),
        })
    }

    fn resolve_target(&self, requested: &str) -> Result<TargetModel, GatewayError> {
        // A group answers to its own name before anything else: it is the
        // model id an operator handed out, and it must not be shadowed by
        // a provider that happens to serve a model of the same name.
        if let Some(group) = self.groups.get(requested) {
            return Ok(weighted_pick_target(&group.primary));
        }
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
                "unknown model `{requested}`; use `provider/model`, a routing group, \
                 a configured alias, or a model listed in a key's `models`"
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

    /// The chain a target falls back through, if one is configured.
    pub fn fallbacks_for(&self, target: &TargetModel) -> Option<&Vec<TargetModel>> {
        self.fallbacks.get(target)
    }

    pub fn groups(&self) -> &BTreeMap<String, RoutingGroup> {
        &self.groups
    }

    pub fn group(&self, name: &str) -> Option<&RoutingGroup> {
        self.groups.get(name)
    }

    pub fn catalog(&self) -> &BTreeMap<String, String> {
        &self.catalog
    }
}

/// The same key in the outgoing snapshot, matched on provider and key
/// name — the only identity a config edit preserves.
fn previous_key<'a>(
    prev: Option<&'a RoutingTable>,
    provider: &str,
    key: &str,
) -> Option<&'a KeyRuntime> {
    prev?
        .providers
        .get(provider)?
        .keys
        .iter()
        .find(|k| k.name == key)
}

/// Keep the running balance when a limit is unchanged in kind; start
/// fresh when one is newly added or its shape changed.
fn carry_bucket(prev: Option<&TokenBucket>, limit: Option<u64>) -> Option<TokenBucket> {
    match (prev, limit) {
        (Some(bucket), Some(_)) => Some(bucket.clone_state()),
        (_, Some(limit)) => Some(TokenBucket::new(limit, limit.div_ceil(60))),
        (_, None) => None,
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
            .filter(|k| k.breaker.looks_healthy(now_ms))
            .collect();
        // Weighted pick among the healthy, skipping any key that is over
        // its own rate ceiling. A rate-limited key is not *unhealthy* —
        // its breaker is closed and it will serve again shortly — so it is
        // stepped over rather than recorded as a failure.
        let mut candidates = healthy;
        while !candidates.is_empty() {
            let picked = weighted_pick(&candidates);
            if picked.try_admit_request(now_ms) {
                return Some(KeyChoice {
                    key: Some(picked),
                    admission: Admission::Yes,
                });
            }
            candidates.retain(|k| !std::ptr::eq(*k, picked));
        }

        // Nothing looked healthy. `admit` is the authority and it also
        // clears a bench that has elapsed, so a seat whose window rolled
        // between the filter above and here answers `Yes` — take it.
        // Insisting on `Probe` here used to drop that seat on the floor
        // and report the whole pool exhausted.
        for key in eligible {
            match key.breaker.admit(now_ms) {
                Admission::No => continue,
                admission => {
                    if admission == Admission::Yes && !key.try_admit_request(now_ms) {
                        continue;
                    }
                    return Some(KeyChoice {
                        key: Some(key),
                        admission,
                    });
                }
            }
        }
        None
    }

    /// How many eligible keys could serve `model` right now.
    ///
    /// The dispatch loop sizes its retry budget from this. A fixed budget
    /// of two attempts is a sane default for a pool of two metered keys
    /// and badly wrong for a subscription pool of ninety seats: one bad
    /// seat would end the request while eighty-eight healthy ones sat
    /// idle. Counting the *healthy* keys rather than all of them keeps the
    /// budget honest — retrying more times than there are seats to try
    /// only burns the caller's latency.
    pub fn healthy_key_count(&self, model: &str, now_ms: u64) -> u32 {
        if self.keys.is_empty() {
            return 1;
        }
        self.keys
            .iter()
            .filter(|k| k.models.as_ref().is_none_or(|m| m.contains(model)))
            .filter(|k| k.breaker.looks_healthy(now_ms))
            .count()
            .max(1) as u32
    }

    /// Whether every eligible key for `model` is benched on a quota
    /// window the provider itself declared.
    ///
    /// This distinguishes the two ways a provider can have nothing to
    /// offer, which deserve different answers: a broken provider is a
    /// capacity problem the caller should retry against, while an
    /// exhausted subscription pool is a rate limit with a known reset —
    /// and telling a caller "no capacity" when the truth is "out of quota
    /// until Tuesday" sends them into a retry loop that cannot succeed.
    pub fn all_keys_benched(&self, model: &str, now_ms: u64) -> bool {
        let mut eligible = self
            .keys
            .iter()
            .filter(|k| k.models.as_ref().is_none_or(|m| m.contains(model)))
            .peekable();
        // Only benches that are still running count. Answering "out of
        // quota until Tuesday" off a deadline that passed on Sunday sends
        // the caller away from a pool that is ready to serve.
        eligible.peek().is_some() && eligible.all(|k| k.breaker.is_benched(now_ms))
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

/// Draw a whole pool in weighted-random order, without replacement.
///
/// Efraimidis–Spirakis: give each entry the key `u^(1/w)` for a uniform
/// `u`, then sort descending. The head comes out with probability exactly
/// proportional to its weight — which is the traffic split — and each
/// subsequent position is the same draw over what is left, so a failover
/// walks the pool in an order that still respects the weights.
fn weighted_order(pool: &[WeightedTarget]) -> Vec<TargetModel> {
    if pool.len() < 2 {
        return pool.iter().map(|w| w.target.clone()).collect();
    }
    let mut keyed: Vec<(f64, &TargetModel)> = pool
        .iter()
        .map(|w| {
            // `f64()` is [0, 1); an exact zero would sort every entry it
            // hits to the back regardless of weight.
            let u = fastrand::f64().max(f64::MIN_POSITIVE);
            (u.powf(1.0 / w.weight), &w.target)
        })
        .collect();
    keyed.sort_by(|a, b| b.0.total_cmp(&a.0));
    keyed.into_iter().map(|(_, t)| t.clone()).collect()
}

/// One weighted draw from a pool, for the callers that need a single
/// target rather than a dispatch order.
fn weighted_pick_target(pool: &[WeightedTarget]) -> TargetModel {
    let total: f64 = pool.iter().map(|w| w.weight).sum();
    let mut roll = fastrand::f64() * total;
    for entry in pool {
        roll -= entry.weight;
        if roll <= 0.0 {
            return entry.target.clone();
        }
    }
    pool.last()
        .expect("a validated group has a non-empty primary pool")
        .target
        .clone()
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

/// Turn a configured key value into the credential the provider needs.
///
/// A metered provider's key is used verbatim. A subscription provider's
/// key is a *document* — a Codex `auth.json`, a Claude Code credential —
/// and is parsed into a renewable seat. A document we cannot parse
/// degrades to an inline, non-renewable token rather than failing the
/// whole config: an operator who pasted a bare `claude setup-token` value
/// has given us something perfectly usable, just not something that can
/// refresh itself.
fn build_credential(kind: ProviderKind, secret: &SecretString) -> Credential {
    let raw = secret.expose();
    let parsed = match kind {
        ProviderKind::CodexSubscription => credential::parse_codex_auth_json(raw),
        ProviderKind::ClaudeSubscription => {
            credential::parse_claude_oauth_json(raw).or_else(|_| credential::inline_token(raw))
        }
        _ => return Credential::Static(secret.clone()),
    };
    match parsed.or_else(|_| credential::inline_token(raw)) {
        Ok(state) => Credential::Seat(Arc::new(Seat::new(state))),
        // An empty value; config validation already rejects those, so this
        // is unreachable in practice and a static credential is the least
        // surprising fallback.
        Err(_) => Credential::Static(secret.clone()),
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

    fn config(toml: &str) -> Config {
        Config::from_str_with_env(toml, Format::Toml, &|_: &str| None).unwrap()
    }

    const LIMITED: &str = r#"
[providers.openai]
keys = [
  { name = "slow", value = "sk-a", rpm = 60 },
  { name = "spare", value = "sk-b" },
]
"#;

    #[test]
    fn a_key_over_its_own_rpm_steps_aside_for_the_next() {
        let t = table(LIMITED);
        let p = t.providers.get("openai").unwrap();
        // 60 rpm = a 60-token bucket refilling one per second. Drain it
        // within the same millisecond so refill cannot mask the limit.
        let slow = p.keys.iter().find(|k| k.name == "slow").unwrap();
        for _ in 0..60 {
            assert!(slow.try_admit_request(1_000));
        }
        assert!(
            !slow.try_admit_request(1_000),
            "the ceiling must actually bind"
        );

        // Selection still serves, because the other key is unlimited.
        for _ in 0..20 {
            let choice = p.admit_key("gpt-4o", 1_000).expect("a key is admitted");
            assert_eq!(choice.key.unwrap().name, "spare");
        }
    }

    #[test]
    fn a_rate_limited_key_is_not_treated_as_unhealthy() {
        let t = table(LIMITED);
        let p = t.providers.get("openai").unwrap();
        let slow = p.keys.iter().find(|k| k.name == "slow").unwrap();
        for _ in 0..60 {
            slow.try_admit_request(1_000);
        }
        // Being out of allowance is not a fault: nothing failed upstream,
        // and the key must come back on its own without a probe.
        assert!(slow.breaker.looks_healthy(61_000));
        assert!(
            slow.try_admit_request(61_000),
            "a minute later the allowance has refilled"
        );
    }

    #[test]
    fn reloading_the_config_does_not_refill_a_spent_allowance() {
        // Otherwise "edit the config" is a way around the ceiling, and a
        // config that reloads often never enforces one at all.
        let first = table(LIMITED);
        let spent = first.providers.get("openai").unwrap();
        let slow = spent.keys.iter().find(|k| k.name == "slow").unwrap();
        for _ in 0..60 {
            slow.try_admit_request(1_000);
        }
        assert!(!slow.try_admit_request(1_000));

        let rebuilt = RoutingTable::from_config_with(&config(LIMITED), Some(&first));
        let carried = rebuilt
            .providers
            .get("openai")
            .unwrap()
            .keys
            .iter()
            .find(|k| k.name == "slow")
            .unwrap();
        assert!(
            !carried.try_admit_request(1_000),
            "the spent allowance must survive the swap"
        );
    }

    #[test]
    fn a_newly_added_limit_starts_full_and_a_removed_one_disappears() {
        let first = table(LIMITED);
        let added = RoutingTable::from_config_with(
            &config(
                r#"
[providers.openai]
keys = [
  { name = "slow", value = "sk-a", rpm = 60 },
  { name = "spare", value = "sk-b", rpm = 10 },
]
"#,
            ),
            Some(&first),
        );
        let spare = added
            .providers
            .get("openai")
            .unwrap()
            .keys
            .iter()
            .find(|k| k.name == "spare")
            .unwrap();
        assert_eq!(spare.rate_headroom().0, Some(10));

        let removed = RoutingTable::from_config_with(&config(BASE), Some(&first));
        let a = removed
            .providers
            .get("openai")
            .unwrap()
            .keys
            .iter()
            .find(|k| k.name == "a")
            .unwrap();
        assert_eq!(a.rate_headroom(), (None, None));
    }

    #[test]
    fn an_observed_quota_survives_a_reload() {
        // A reload must not make the console claim it knows nothing about
        // a seat that is, in fact, still exhausted.
        let first = table(LIMITED);
        let key = &first.providers.get("openai").unwrap().keys[0];
        key.observe_quota(
            crate::quota::Quota {
                primary: Some(crate::quota::Window {
                    utilization: 0.91,
                    resets_in: Some(std::time::Duration::from_secs(600)),
                    length: None,
                    rejected: false,
                }),
                secondary: None,
            },
            42,
        );
        let rebuilt = RoutingTable::from_config_with(&config(LIMITED), Some(&first));
        let carried = rebuilt.providers.get("openai").unwrap().keys[0]
            .quota()
            .expect("the reading is carried across the swap");
        assert_eq!(carried.observed_ms, 42);
        assert_eq!(carried.quota.peak_utilization(), Some(0.91));
    }

    #[test]
    fn an_empty_quota_reading_is_not_recorded() {
        // A payload we no longer recognise must read as "no information",
        // not as a seat with 0% utilization.
        let t = table(LIMITED);
        let key = &t.providers.get("openai").unwrap().keys[0];
        key.observe_quota(crate::quota::Quota::default(), 42);
        assert!(key.quota().is_none());
    }

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

    /// A pool is only worth its healthy seats, and the dispatch loop
    /// sizes its retry budget from this. Benched seats must not count:
    /// budgeting attempts for seats that cannot serve spends the
    /// caller's latency on picks that are refused before they leave.
    #[test]
    fn the_healthy_count_tracks_the_pool() {
        let t = table(
            r#"
[providers.openai]
keys = [
  { name = "a", value = "sk-a" },
  { name = "b", value = "sk-b" },
  { name = "c", value = "sk-c" },
]
"#,
        );
        let r = t.resolve("openai/gpt-4o").unwrap();
        assert_eq!(r.provider.healthy_key_count("gpt-4o", 0), 3);

        r.provider.keys[0].breaker.bench_until(10_000);
        r.provider.keys[1].breaker.bench_until(10_000);
        assert_eq!(r.provider.healthy_key_count("gpt-4o", 5_000), 1);
        assert!(!r.provider.all_keys_benched("gpt-4o", 5_000));

        // Every seat out: a rate limit, not a capacity problem.
        r.provider.keys[2].breaker.bench_until(10_000);
        assert!(r.provider.all_keys_benched("gpt-4o", 5_000));
        assert_eq!(
            r.provider.healthy_key_count("gpt-4o", 5_000),
            1,
            "floors at 1"
        );

        // Once the windows roll, the pool is whole again without anyone
        // having had to fail a request to discover it.
        assert!(!r.provider.all_keys_benched("gpt-4o", 10_000));
        assert_eq!(r.provider.healthy_key_count("gpt-4o", 10_000), 3);
    }

    /// The seat whose bench just elapsed has to be *returned*, not merely
    /// un-benched. The recovery loop only accepted `Probe`, so a pool of
    /// freshly recovered seats answered "nothing admitted" and the caller
    /// saw a 429 against a pool that was ready.
    #[test]
    fn a_pool_coming_off_the_bench_admits_immediately() {
        let t = table(
            r#"
[providers.openai]
keys = [{ name = "only", value = "sk-a" }]
"#,
        );
        let r = t.resolve("openai/gpt-4o").unwrap();
        r.provider.keys[0].breaker.bench_until(10_000);
        assert!(r.provider.admit_key("gpt-4o", 9_999).is_none());

        let choice = r
            .provider
            .admit_key("gpt-4o", 10_000)
            .expect("the window rolled; the seat serves again");
        assert_eq!(choice.key.unwrap().name, "only");
        assert_eq!(choice.admission, Admission::Yes);
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
