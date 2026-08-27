//! Virtual keys: the gateway's own credentials.
//!
//! A virtual key is `ck-{id}-{secret}`. The store keeps only
//! `id -> BLAKE3(secret)` plus attributes — a stolen store or backup yields
//! no usable credentials. Verification is one map lookup, one hash, and one
//! constant-time compare on the hot path's auth layer.
//!
//! Enforcement order (before any upstream work):
//! key valid/enabled/unexpired -> model in scope -> rate limits -> budget.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;

use crate::sync::{AtomicU64, Ordering};
use crate::token_bucket::TokenBucket;

/// A key definition as persisted (store or file config). Contains no secret
/// material — only hashes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct VirtualKeyDef {
    pub id: String,
    pub name: String,
    /// `blake3:<64 hex chars>`.
    pub secret_hash: String,
    /// Previous secret honored during a rotation grace window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prev_secret: Option<PrevSecret>,
    /// Allowlist of models and/or aliases; empty = all configured models.
    #[serde(default)]
    pub models: Vec<String>,
    /// Which service this key belongs to.
    ///
    /// A tenant is a *service* — the AGI gateway, the Slack agent, the
    /// optimizer — so many keys can belong to one, and a floor survives a
    /// key rotation. It decides how deep into an account pool this key may
    /// draw while the pool is under pressure; `None` is served last, behind
    /// every declared service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tenant: Option<String>,
    /// Whether this key may be handed an account's credential to spend
    /// directly, rather than only spending it through the gateway.
    ///
    /// Off by default, and deliberately a separate switch from naming a
    /// service: using an account through the gateway is weaker than
    /// holding it. Only a caller that must drive a vendor CLI — which
    /// cannot be pointed at us — should be able to take one.
    #[serde(default, skip_serializing_if = "is_false")]
    pub lease_accounts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budget: Option<Budget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<RateLimit>,
    /// Unix milliseconds; absent = never expires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_ms: Option<u64>,
    #[serde(default)]
    pub tags: BTreeMap<String, String>,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub created_ms: u64,
}

fn default_true() -> bool {
    true
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PrevSecret {
    pub secret_hash: String,
    /// Unix milliseconds after which the old secret stops verifying.
    pub valid_until_ms: u64,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Budget {
    pub usd: f64,
    pub period: BudgetPeriod,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum BudgetPeriod {
    Daily,
    Weekly,
    Monthly,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RateLimit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u64>,
    /// Tokens per minute, input + output (provider-cached tokens excluded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpm: Option<u64>,
}

/// Freshly generated key material. The full token exists only in this value
/// and is shown exactly once.
pub struct GeneratedKey {
    pub id: String,
    pub secret: String,
}

impl GeneratedKey {
    pub fn full(&self) -> String {
        format!("ck-{}-{}", self.id, self.secret)
    }
}

const SECRET_LEN: usize = 26;
const ID_LEN: usize = 6;
const BASE62: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

pub fn generate() -> GeneratedKey {
    let id: String = (0..ID_LEN)
        .map(|_| char::from_digit(fastrand::u32(0..16), 16).unwrap())
        .collect();
    GeneratedKey {
        id,
        secret: generate_secret(),
    }
}

pub fn generate_secret() -> String {
    (0..SECRET_LEN)
        .map(|_| BASE62[fastrand::usize(0..BASE62.len())] as char)
        .collect()
}

pub fn hash_secret(secret: &str) -> String {
    format!("blake3:{}", blake3::hash(secret.as_bytes()).to_hex())
}

/// Split a presented token into `(id, secret)` if it has the `ck-` shape.
/// Anything else is not a virtual key (it may be a static gateway key).
pub fn parse(token: &str) -> Option<(&str, &str)> {
    let rest = token.strip_prefix("ck-")?;
    let (id, secret) = rest.split_once('-')?;
    if id.len() != ID_LEN || !id.bytes().all(|b| b.is_ascii_hexdigit()) || secret.is_empty() {
        return None;
    }
    Some((id, secret))
}

fn decode_hash(stored: &str) -> Option<[u8; 32]> {
    let hex = stored.strip_prefix("blake3:")?;
    if hex.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(hex.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(out)
}

/// Why a presented key was rejected. All variants map to the same 401 on
/// the wire — the distinction exists for metrics and logs only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VkReject {
    Unknown,
    BadSecret,
    Disabled,
    Expired,
}

/// What a request was denied for after authentication.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VkDeny {
    RateLimited { retry_after_secs: u64 },
    BudgetExhausted,
}

/// Per-key spend accounting for the current budget period. Carried across
/// table rebuilds so config edits never reset a period's spend.
///
/// Period and spend live in one packed word — `(period << 40) | micro_usd`
/// — so a period rollover and a debit commit in a single CAS. A two-word
/// design (roll-then-add) loses debits that land between the roll's CAS
/// and its reset store; loom found that interleaving, this is the fix.
#[derive(Debug, Default)]
pub struct SpendState {
    packed: AtomicU64,
}

/// 40 bits of micro-USD (~$1.1M per period, saturating) leaves 24 bits of
/// period ordinal — compared modulo 2^24, which two real timestamps can
/// only collide on 45,000 years apart.
const SPEND_BITS: u64 = 40;
const SPEND_MASK: u64 = (1 << SPEND_BITS) - 1;
const PERIOD_MASK: u64 = (1 << 24) - 1;

impl SpendState {
    pub fn add(&self, micro_usd: u64, period_ordinal: u64) {
        let period = period_ordinal & PERIOD_MASK;
        let mut current = self.packed.load(Ordering::Acquire);
        loop {
            let (p, spent) = (current >> SPEND_BITS, current & SPEND_MASK);
            let next_spend = if p == period {
                spent.saturating_add(micro_usd).min(SPEND_MASK)
            } else {
                // New period: the debit itself is the opening balance.
                micro_usd.min(SPEND_MASK)
            };
            let next = (period << SPEND_BITS) | next_spend;
            match self.packed.compare_exchange_weak(
                current,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn current(&self, period_ordinal: u64) -> u64 {
        let current = self.packed.load(Ordering::Acquire);
        if current >> SPEND_BITS == period_ordinal & PERIOD_MASK {
            current & SPEND_MASK
        } else {
            0 // stale period: spend rolls to zero lazily on the next add
        }
    }
}

/// A key's runtime state: definition, decoded hashes, and enforcement
/// atomics.
#[derive(Debug)]
pub struct VkRuntime {
    pub def: VirtualKeyDef,
    hash: Option<[u8; 32]>,
    prev: Option<([u8; 32], u64)>,
    pub rpm: Option<TokenBucket>,
    pub tpm: Option<TokenBucket>,
    pub spend: Arc<SpendState>,
    /// Live member count this key's buckets were sized for.
    shares: u64,
}

impl VkRuntime {
    fn new(def: VirtualKeyDef, prev_rt: Option<&VkRuntime>, shares: usize) -> Self {
        let hash = decode_hash(&def.secret_hash);
        let prev = def
            .prev_secret
            .as_ref()
            .and_then(|p| Some((decode_hash(&p.secret_hash)?, p.valid_until_ms)));
        let rate = def.rate.unwrap_or_default();
        // Buckets carry over only while their limits are unchanged; spend
        // always carries over.
        // A key's limit is fleet-wide; each node enforces its share of it,
        // so the denominator is the live member count. One node means one
        // share, which is the single-box case unchanged.
        let shares = shares.max(1) as u64;
        let share = |limit: Option<u64>| limit.map(|l| (l / shares).max(1));
        let (share_rpm, share_tpm) = (share(rate.rpm), share(rate.tpm));

        let reuse_buckets = prev_rt.is_some_and(|p| p.def.rate == def.rate && p.shares == shares);
        let (rpm, tpm) = if reuse_buckets {
            let p = prev_rt.unwrap();
            (
                take_bucket(&p.rpm, share_rpm),
                take_bucket(&p.tpm, share_tpm),
            )
        } else {
            (new_bucket(share_rpm), new_bucket(share_tpm))
        };
        let spend = prev_rt
            .map(|p| p.spend.clone())
            .unwrap_or_else(|| Arc::new(SpendState::default()));
        Self {
            def,
            hash,
            prev,
            rpm,
            tpm,
            spend,
            shares,
        }
    }

    /// Constant-time secret verification; the BLAKE3 digest equalizes
    /// lengths before the compare.
    fn verify_secret(&self, secret: &str, unix_ms: u64) -> bool {
        let candidate = *blake3::hash(secret.as_bytes()).as_bytes();
        if let Some(hash) = &self.hash
            && bool::from(candidate.ct_eq(hash))
        {
            return true;
        }
        if let Some((prev_hash, valid_until)) = &self.prev
            && unix_ms <= *valid_until
            && bool::from(candidate.ct_eq(prev_hash))
        {
            return true;
        }
        false
    }

    /// Scope check: the allowlist may name the requested string (model or
    /// alias) or the resolved `provider/model` target.
    pub fn allows_model(&self, requested: &str, resolved: Option<&str>) -> bool {
        if self.def.models.is_empty() {
            return true;
        }
        self.def
            .models
            .iter()
            .any(|m| m == requested || resolved.is_some_and(|r| m == r))
    }

    /// Pre-flight admission: rpm consumes one request; tpm and budget are
    /// threshold checks debited after the response (post-paid).
    pub fn admit(&self, mono_ms: u64, period_ordinal: u64) -> Result<(), VkDeny> {
        if let Some(rpm) = &self.rpm
            && !rpm.try_consume(1, mono_ms)
        {
            let per_min = (self.def.rate.and_then(|r| r.rpm).unwrap_or(60) / self.shares).max(1);
            return Err(VkDeny::RateLimited {
                retry_after_secs: (60 / per_min).clamp(1, 60),
            });
        }
        if let Some(tpm) = &self.tpm
            && tpm.available_tokens() == 0
        {
            // Refill happens on consume; poke it with a zero-cost consume.
            let _ = tpm.try_consume(0, mono_ms);
            if tpm.available_tokens() == 0 {
                return Err(VkDeny::RateLimited {
                    retry_after_secs: 1,
                });
            }
        }
        if self.budget_exhausted(period_ordinal) {
            return Err(VkDeny::BudgetExhausted);
        }
        Ok(())
    }

    pub fn budget_exhausted(&self, period_ordinal: u64) -> bool {
        let Some(budget) = &self.def.budget else {
            return false;
        };
        let limit_micro = (budget.usd * 1_000_000.0).max(0.0) as u64;
        self.spend.current(period_ordinal) >= limit_micro
    }

    /// Debit actual usage after the response: tokens against the tpm
    /// bucket (draining at most what is available) and spend against the
    /// budget.
    pub fn debit_usage(&self, tokens: u64, micro_usd: u64, mono_ms: u64, period_ordinal: u64) {
        if let (Some(tpm), true) = (&self.tpm, tokens > 0) {
            tpm.debit_saturating(tokens, mono_ms);
        }
        if micro_usd > 0 {
            self.spend.add(micro_usd, period_ordinal);
        }
    }

    /// The budget-period ordinal for a wall-clock instant.
    pub fn period_ordinal(&self, unix_ms: u64) -> u64 {
        let period = self
            .def
            .budget
            .map(|b| b.period)
            .unwrap_or(BudgetPeriod::Monthly);
        period_ordinal(period, unix_ms)
    }
}

fn new_bucket(per_min: Option<u64>) -> Option<TokenBucket> {
    // A per-minute limit refills continuously at limit/60 per second (at
    // least 1), with burst capacity of the full minute's allowance.
    per_min.map(|limit| TokenBucket::new(limit, (limit / 60).max(1)))
}

fn take_bucket(prev: &Option<TokenBucket>, limit: Option<u64>) -> Option<TokenBucket> {
    match (prev, limit) {
        (Some(b), Some(_)) => Some(b.clone_state()),
        _ => new_bucket(limit),
    }
}

pub fn period_ordinal(period: BudgetPeriod, unix_ms: u64) -> u64 {
    let days = unix_ms / 86_400_000;
    match period {
        BudgetPeriod::Daily => days,
        BudgetPeriod::Weekly => days / 7,
        BudgetPeriod::Monthly => {
            let (year, month, _) = civil_from_days(days as i64);
            (year as u64) * 12 + (month as u64 - 1)
        }
    }
}

/// Strict RFC 3339 UTC (`YYYY-MM-DDTHH:MM:SSZ`, optional fractional
/// seconds) to unix milliseconds. Non-UTC offsets are rejected — expiry
/// timestamps in configs must be unambiguous.
pub fn parse_rfc3339_utc_ms(s: &str) -> Option<u64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut date_parts = date.split('-');
    let year: i64 = date_parts.next()?.parse().ok()?;
    let month: u32 = date_parts.next()?.parse().ok()?;
    let day: u32 = date_parts.next()?.parse().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let (hms, frac) = match time.split_once('.') {
        Some((hms, frac)) => (hms, frac),
        None => (time, ""),
    };
    let mut time_parts = hms.split(':');
    let hour: u64 = time_parts.next()?.parse().ok()?;
    let minute: u64 = time_parts.next()?.parse().ok()?;
    let second: u64 = time_parts.next()?.parse().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let millis = if frac.is_empty() {
        0
    } else {
        let digits: String = frac.chars().take(3).collect();
        if !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        format!("{digits:0<3}").parse::<u64>().ok()?
    };
    let days = days_from_civil(year, month, day);
    if days < 0 {
        return None;
    }
    Some((days as u64) * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1_000 + millis)
}

/// Civil (year, month, day) to days-since-epoch.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) as i64 + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days-since-epoch to civil (year, month, day).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
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

/// The immutable lookup table swapped in whole on every config/store apply.
#[derive(Debug)]
pub struct VkTable {
    by_id: HashMap<String, Arc<VkRuntime>>,
    shares: usize,
}

impl Default for VkTable {
    fn default() -> Self {
        Self {
            by_id: HashMap::new(),
            shares: 1,
        }
    }
}

impl VkTable {
    /// Build a table for a single node.
    pub fn build(defs: &[VirtualKeyDef], prev: Option<&VkTable>) -> Self {
        Self::build_with_shares(defs, prev, 1)
    }

    /// Build a table, carrying enforcement state (buckets, spend) over from
    /// the previous table by key id, with each rate limit divided across
    /// `shares` live nodes.
    pub fn build_with_shares(
        defs: &[VirtualKeyDef],
        prev: Option<&VkTable>,
        shares: usize,
    ) -> Self {
        let mut by_id = HashMap::with_capacity(defs.len());
        for def in defs {
            let prev_rt = prev.and_then(|t| t.by_id.get(&def.id)).map(Arc::as_ref);
            by_id.insert(
                def.id.clone(),
                Arc::new(VkRuntime::new(def.clone(), prev_rt, shares)),
            );
        }
        Self {
            by_id,
            shares: shares.max(1),
        }
    }

    /// The live member count these buckets are sized for.
    pub fn shares(&self) -> usize {
        self.shares
    }

    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    pub fn get(&self, id: &str) -> Option<&Arc<VkRuntime>> {
        self.by_id.get(id)
    }

    pub fn iter(&self) -> impl Iterator<Item = &Arc<VkRuntime>> {
        self.by_id.values()
    }

    /// Authenticate a presented token. `Err` reasons are for telemetry;
    /// the wire response is a uniform 401.
    pub fn verify(&self, token: &str, unix_ms: u64) -> Result<Arc<VkRuntime>, VkReject> {
        let Some((id, secret)) = parse(token) else {
            return Err(VkReject::Unknown);
        };
        let Some(rt) = self.by_id.get(id) else {
            // Burn a hash so unknown ids cost the same as bad secrets.
            let _ = blake3::hash(secret.as_bytes());
            return Err(VkReject::Unknown);
        };
        if !rt.verify_secret(secret, unix_ms) {
            return Err(VkReject::BadSecret);
        }
        if !rt.def.enabled {
            return Err(VkReject::Disabled);
        }
        if rt.def.expires_ms.is_some_and(|exp| unix_ms >= exp) {
            return Err(VkReject::Expired);
        }
        Ok(rt.clone())
    }
}

/// Wall-clock unix milliseconds (budgets, expiry, rotation grace live in
/// real time, unlike the monotonic bucket clock).
pub fn unix_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    fn def(id: &str, secret: &str) -> VirtualKeyDef {
        VirtualKeyDef {
            id: id.into(),
            name: format!("key-{id}"),
            secret_hash: hash_secret(secret),
            prev_secret: None,
            models: Vec::new(),
            tenant: None,
            lease_accounts: false,
            budget: None,
            rate: None,
            expires_ms: None,
            tags: BTreeMap::new(),
            enabled: true,
            created_ms: 0,
        }
    }

    #[test]
    fn generated_keys_parse_and_verify() {
        let generated = generate();
        let token = generated.full();
        let (id, secret) = parse(&token).expect("generated key parses");
        assert_eq!(id, generated.id);
        assert_eq!(secret, generated.secret);

        let table = VkTable::build(&[def(&generated.id, &generated.secret)], None);
        assert!(table.verify(&token, 0).is_ok());
        assert_eq!(
            table
                .verify(&format!("ck-{}-wrong", generated.id), 0)
                .unwrap_err(),
            VkReject::BadSecret
        );
    }

    #[test]
    fn parse_rejects_malformed_tokens() {
        assert!(parse("sk-abc").is_none());
        assert!(parse("ck-").is_none());
        assert!(parse("ck-abc").is_none()); // no secret separator
        assert!(parse("ck-zzzzzz-secret").is_none()); // non-hex id
        assert!(parse("ck-abc123-").is_none()); // empty secret
        assert!(parse("ck-abcd12-Vv8kJq0R2mX7pT4wN6bY1sD5").is_some());
    }

    #[test]
    fn disabled_expired_and_unknown_reject() {
        let mut d = def("aaaaaa", "s3cret");
        d.enabled = false;
        let table = VkTable::build(&[d], None);
        assert_eq!(
            table.verify("ck-aaaaaa-s3cret", 0).unwrap_err(),
            VkReject::Disabled
        );

        let mut d = def("bbbbbb", "s3cret");
        d.expires_ms = Some(1_000);
        let table = VkTable::build(&[d], None);
        assert!(table.verify("ck-bbbbbb-s3cret", 999).is_ok());
        assert_eq!(
            table.verify("ck-bbbbbb-s3cret", 1_000).unwrap_err(),
            VkReject::Expired
        );

        assert_eq!(
            table.verify("ck-cccccc-nope", 0).unwrap_err(),
            VkReject::Unknown
        );
    }

    #[test]
    fn rotation_grace_honors_old_secret_until_deadline() {
        let mut d = def("abc123", "new-secret");
        d.prev_secret = Some(PrevSecret {
            secret_hash: hash_secret("old-secret"),
            valid_until_ms: 5_000,
        });
        let table = VkTable::build(&[d], None);
        assert!(table.verify("ck-abc123-new-secret", 0).is_ok());
        assert!(table.verify("ck-abc123-old-secret", 4_999).is_ok());
        assert_eq!(
            table.verify("ck-abc123-old-secret", 5_001).unwrap_err(),
            VkReject::BadSecret
        );
        assert!(table.verify("ck-abc123-new-secret", 6_000).is_ok());
    }

    #[test]
    fn scope_matches_requested_or_resolved() {
        let mut d = def("abc123", "s");
        d.models = vec!["fast".into(), "openai/gpt-4o-mini".into()];
        let rt = VkRuntime::new(d, None, 1);
        assert!(rt.allows_model("fast", Some("anthropic/claude-haiku-4-5")));
        assert!(rt.allows_model("gpt-4o-mini", Some("openai/gpt-4o-mini")));
        assert!(!rt.allows_model("gpt-4.1", Some("openai/gpt-4.1")));

        let unscoped = VkRuntime::new(def("dddddd", "s"), None, 1);
        assert!(unscoped.allows_model("anything", None));
    }

    #[test]
    fn a_key_carries_the_service_it_belongs_to() {
        let mut d = def("abc123", "s");
        d.tenant = Some("optimizer".into());
        let rt = VkRuntime::new(d, None, 1);
        assert_eq!(rt.def.tenant.as_deref(), Some("optimizer"));
        // Which accounts that reaches is the pool's decision, not the
        // key's: a key names a service, never a credential.
        assert!(rt.allows_model("anything", None));
    }

    #[test]
    fn rpm_limits_and_budget_deny() {
        let mut d = def("abc123", "s");
        d.rate = Some(RateLimit {
            rpm: Some(2),
            tpm: None,
        });
        d.budget = Some(Budget {
            usd: 1.0,
            period: BudgetPeriod::Monthly,
        });
        let rt = VkRuntime::new(d, None, 1);
        assert!(rt.admit(0, 0).is_ok());
        assert!(rt.admit(0, 0).is_ok());
        assert!(matches!(rt.admit(0, 0), Err(VkDeny::RateLimited { .. })));

        rt.debit_usage(0, 999_999, 0, 0);
        // One micro-USD under budget: still admitted (next minute).
        assert!(!rt.budget_exhausted(0));
        rt.debit_usage(0, 1, 0, 0);
        assert!(rt.budget_exhausted(0));
        // New period: spend resets.
        assert!(!rt.budget_exhausted(1));
    }

    #[test]
    fn spend_carries_across_rebuild_and_periods_roll() {
        let mut d = def("abc123", "s");
        d.budget = Some(Budget {
            usd: 0.5,
            period: BudgetPeriod::Daily,
        });
        let table = VkTable::build(&[d.clone()], None);
        let rt = table.get("abc123").unwrap();
        rt.debit_usage(0, 500_000, 0, 7);
        assert!(rt.budget_exhausted(7));

        // Rebuild with an edited name: spend must survive.
        d.name = "renamed".into();
        let table2 = VkTable::build(&[d], Some(&table));
        let rt2 = table2.get("abc123").unwrap();
        assert!(rt2.budget_exhausted(7));
        assert!(!rt2.budget_exhausted(8));
    }

    #[test]
    fn tpm_post_paid_debit_blocks_next_request() {
        let mut d = def("abc123", "s");
        d.rate = Some(RateLimit {
            rpm: None,
            tpm: Some(1_000),
        });
        let rt = VkRuntime::new(d, None, 1);
        assert!(rt.admit(0, 0).is_ok());
        rt.debit_usage(5_000, 0, 0, 0); // response was huge; drain the bucket
        assert!(matches!(rt.admit(1, 0), Err(VkDeny::RateLimited { .. })));
        // A minute later the bucket has refilled.
        assert!(rt.admit(61_000, 0).is_ok());
    }

    #[test]
    fn monthly_ordinal_tracks_civil_months() {
        // 2026-01-31 and 2026-02-01 are different months; 2026-02-28 and
        // 2026-02-01 are the same.
        let jan31 = 1_769_817_600_000u64; // 2026-01-31T00:00:00Z
        let feb1 = jan31 + 86_400_000;
        let feb28 = feb1 + 27 * 86_400_000;
        let mar1 = feb28 + 86_400_000;
        let ord = |ms| period_ordinal(BudgetPeriod::Monthly, ms);
        assert_ne!(ord(jan31), ord(feb1));
        assert_eq!(ord(feb1), ord(feb28));
        assert_ne!(ord(feb28), ord(mar1));
    }

    #[test]
    fn defs_round_trip_serde() {
        let mut d = def("abc123", "s");
        d.budget = Some(Budget {
            usd: 250.0,
            period: BudgetPeriod::Monthly,
        });
        d.rate = Some(RateLimit {
            rpm: Some(600),
            tpm: Some(400_000),
        });
        d.tenant = Some("optimizer".into());
        d.tags.insert("team".into(), "payments".into());
        let json = serde_json::to_string(&d).unwrap();
        let back: VirtualKeyDef = serde_json::from_str(&json).unwrap();
        assert_eq!(d, back);
        assert!(!json.contains("s3cret"));
    }

    #[test]
    fn rfc3339_parses_strict_utc_only() {
        assert_eq!(parse_rfc3339_utc_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_rfc3339_utc_ms("1970-01-02T00:00:00.5Z"),
            Some(86_400_500)
        );
        // 2026-08-15 00:00:00 UTC (cross-checked against civil_from_days).
        let ms = parse_rfc3339_utc_ms("2026-08-15T00:00:00Z").unwrap();
        assert_eq!(civil_from_days((ms / 86_400_000) as i64), (2026, 8, 15));
        assert!(parse_rfc3339_utc_ms("2026-08-15T00:00:00+05:30").is_none());
        assert!(parse_rfc3339_utc_ms("2026-13-01T00:00:00Z").is_none());
        assert!(parse_rfc3339_utc_ms("not-a-date").is_none());
    }
}
