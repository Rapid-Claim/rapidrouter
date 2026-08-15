# Virtual Keys

Virtual keys are the gateway's own credentials: you hand applications,
teams, and machines a **caret key** instead of a provider key. Each key
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
models      = ["openai/gpt-4o-mini", "smart", "fast"]   # models and/or aliases
budget      = { usd = 250, period = "monthly" }
rate_limit  = { rpm = 600, tpm = 400_000 }
expires     = "2027-01-01T00:00:00Z"                     # optional
tags        = { team = "payments" }
enabled     = true
```

- **Scope** is an allowlist of models and/or aliases (aliases keep scopes
  stable while you repoint targets). No scope = all configured models.
- **Budget** is spend per period, priced from provider-reported usage.
- **Rate limits** are requests/min and tokens/min (input + output;
  provider-cached tokens excluded).
- **Tags** flow into usage records and metrics labels are *not* generated
  from them (cardinality stays bounded); they exist for console filtering
  and exports.

## Enforcement semantics

Enforcement happens in the auth layer, before routing, in this order:

| Check | Failure | Notes |
|---|---|---|
| Key exists, enabled, unexpired | `401 authentication` | constant-time verify |
| Model in scope | `403 permission` — names the model and key | checked after model extraction, before any upstream work |
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
(`caret_tokens_total{vkey=…}`) only when `metrics.per_key = true` and the
key count is under a configured cardinality cap.

## Lifecycle

```bash
caret-router key create --name checkout-service \
  --models openai/gpt-4o-mini,fast --budget-usd 250/monthly --rpm 600
# → prints the full key once

caret-router key ls | rotate <id> | disable <id> | rm <id>
```

- **Rotate** issues a new secret for the same id/attributes with an overlap
  window (old secret honored for `rotation_grace`, default 24 h) so
  deployments roll without a hard cut.
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
