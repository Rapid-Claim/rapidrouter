# Virtual Keys

Virtual keys are the gateway's own credentials: you hand applications,
teams, and machines a **rapid key** instead of a provider key. Each key
carries its own scope, budget, and rate limits; provider credentials never
leave the gateway. Rotating a provider key touches one config entry, not a
hundred deployments — and revoking an app's access is one click, not a
provider-key rotation.

## Key format & storage

```
ck-9f3a2c-Vv8kJq0R2mX7pT4wN6bY1sD5
   └─id──┘└──────── secret ───────┘
```

- The full key is shown **exactly once**, at creation. The store keeps only
  `id → BLAKE3(secret)` plus the key's attributes — a stolen store (or
  backup, or replication stream) yields no usable credentials.
- Verification: parse the id, one map lookup, one hash, one constant-time
  compare — nanoseconds, no allocation, on the hot path's auth layer.
- Definitions live in the replicated store and converge across the cluster
  like all control-plane state
  ([../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md));
  in `file` config mode they can equally be declared in the config file
  (hash form) for GitOps shops.

## What a key carries

```toml
[[virtual_keys]]                    # file-mode form; console/CLI is the usual path
name        = "checkout-service"
id          = "9f3a2c"
secret_hash = "blake3:…"
models      = ["openai/gpt-4o-mini", "smart", "fast"]   # models, groups, or aliases
tenant      = "optimizer"                               # which service this key belongs to
budget      = { usd = 250, period = "monthly" }
rate_limit  = { rpm = 600, tpm = 400_000 }
expires     = "2027-01-01T00:00:00Z"                     # optional
tags        = { team = "payments" }
enabled     = true
```

- **Scope** is an allowlist of models, routing groups, and/or aliases (a
  group or alias keeps the scope stable while you repoint what is behind
  it). No scope = all configured models.
- **Tenant** is the service this key belongs to, and decides how deep into
  an account pool it may draw when the pool is under pressure — see below.
- **Budget** is spend per period, priced from provider-reported usage.
- **Rate limits** are requests/min and tokens/min (input + output;
  provider-cached tokens excluded).
- **Tags** flow into usage records and metrics labels are *not* generated
  from them (cardinality stays bounded); they exist for console filtering
  and exports.

## Which accounts a key can spend

`models` answers "what may this key call". The **tenant** answers "which
service is this", and that decides which accounts it may spend.

```toml
tenant = "optimizer"
```

The rule is a match, not an ordering. An account carries the name of the
service that owns it; a key carries the name of the service it is; a request
may spend the accounts whose label matches and **no others**. There is no
borrowing, no priority and no overflow — a service that has exhausted its own
accounts is refused while another service's sit idle. That is the trade the
label buys, and it is the whole mechanism.

A pool where no account is labelled is **shared exactly as before**: every key
reaches every account, tenant or not. This is what makes the feature inert
until someone uses it, and it is why nothing changes for a provider nobody has
divided up.

Once any account in a pool *is* labelled, the pool becomes managed and a key
with **no** tenant reaches nothing there — a `403`, not last place. That is
deliberate: degrading to an unlabelled overflow would quietly re-create
borrowing. It also forces an ordering during rollout, because traffic on a
static `server.auth_keys` gateway key can never carry a tenant at all: **every
caller must be moved onto a `ck-` key before the first account is labelled.**

Many keys may share one tenant, which is the point: a service's allocation
survives issuing, rotating and revoking the keys that spend it. Tenant names
are checked against the `tenants` roster wherever a key is written, so a typo
fails there rather than producing a key that looks fine and owns nothing.

## Enforcement semantics

Enforcement happens in the auth layer, before routing, in this order:

| Check | Failure | Notes |
|---|---|---|
| Key exists, enabled, unexpired | `401 authentication` | constant-time verify |
| Model in scope | `403 permission` — names the model and key | checked after model extraction, before any upstream work |
| Pool pressure | `429 rate_limited` — names the service and the counts | applied at credential selection, per attempt, per target |
| Rate limits | `429 rate_limited` + `retry-after` | atomic token buckets; race-free under concurrency (loom/proptest-verified) |
| Budget | `429 insufficient_quota` | see lag note below |

Cluster behavior, honestly stated:

- **Rate limits**: per-node buckets with live-`N` shares — a key's `rpm`
  divides across cluster members automatically as membership changes.
- **Budgets**: enforced from usage aggregation (local ring + peer
  summaries). Cutoff lag is bounded by the flush/gossip interval — minutes
  at worst, the right trade for budgets and the documented cost of "no
  database." A key at 100.4 % of budget was never a security incident; a
  hard-down gateway was.

## Attribution

Every usage record and request log line carries the key id (never the
secret). The console's per-key views — spend against budget, tokens, error
rates, model mix — and `/admin/api/keys/{id}/usage` are aggregations of
those records. Metrics expose bounded per-key counters
(`rapid_tokens_total{vkey=…}`) only when `metrics.per_key = true` and the
key count is under a configured cardinality cap.

## Lifecycle

```bash
rapid-router key create --name checkout-service \
  --models openai/gpt-4o-mini,fast --budget-usd 250/monthly --rpm 600

rapid-router key create --name bulk-batch --tenant optimizer
# → prints the full key once

rapid-router key ls | rotate <id> | disable <id> | enable <id> | rm <id>

rapid-router key hash            # a secret_hash for a file-mode entry
```

- **Rotate** issues a new secret for the same id/attributes with an overlap
  window (`--grace-hours`, default 24 h) so deployments roll without a hard
  cut. Both secrets verify until the window closes; then only the new one
  does.
- **Revoke** (disable/rm) propagates via the store — effective on every
  node within consensus round-trips, no restart.
- All of the above is equally available in the console's Keys page and the
  admin API; creation always displays the secret exactly once, wherever it
  happens.

## Relationship to gateway auth

`server.auth_keys` (static keys in config) remains the minimal-setup path —
in effect an unscoped virtual key defined by hand. `require_auth = true`
plus virtual keys is the intended multi-tenant posture: every data-plane
request presents a `ck-…` key; anonymous access exists only for
explicitly-open localhost use.
