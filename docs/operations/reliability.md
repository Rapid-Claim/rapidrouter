# Reliability — Operator's View

The mechanics live in [../components/router.md](../components/router.md);
this page is how they behave in production and how to tune them.

## Timeout ladder

| Timeout | Default | Applies to |
|---|---|---|
| Connect | 2 s | TCP+TLS to provider |
| First byte (TTFT) | 30 s | request sent → first response byte |
| Total | 120 s (config/provider) | whole request incl. stream |
| Header read / idle (inbound) | server defaults | slowloris shedding |
| Drain | 30 s | graceful shutdown |

All propagate from a per-request deadline; client disconnects cancel
upstream work immediately.

## What happens when a provider degrades

1. Failures (connect/timeout/5xx/429) count against the specific key.
2. Key breaker opens after 5 qualifying failures in 30 s → traffic shifts
   to remaining keys (weights renormalized by masking).
3. All keys open → fallback chain serves; `caret_fallbacks_total{from,to}`
   and `x-caret-provider` disclose it.
4. Nothing healthy → fast `503` with `retry-after`, never a hang.
5. Cooldown (15 s) → half-open probe → recovery is automatic and gradual.

Watch: `caret_key_state`, `caret_fallbacks_total`, `caret_retries_total`,
`caret_upstream_duration_seconds`.

## Retry policy (gateway-side)

Only failures that are safe to replay advance the candidate list: connect
errors, 429s, and 5xx **before any response byte**. Streams that have
started never fail over — the error surfaces as a terminal stream event.
Client retries then re-enter routing with fresh health state.

## Rate limits: theirs and yours

- Upstream 429s mask the key (with `retry-after` respected) so a throttled
  key rests instead of burning attempts.
- Gateway-side token buckets (per gateway key) count input+output tokens,
  exclude provider-cached tokens, and hold under concurrent racing — these
  invariants are model-checked in CI, not assumed.
- Limits are enforced per node; in cluster mode each node's share tracks
  the live member count automatically (`limit / N`), rescaling on
  join/leave/failure. In stateless-replica mode, size per-node limits with
  the replica count in mind
  ([../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md)).

## Failure modes, honestly

| Scenario | Behavior |
|---|---|
| Bad config (file reload or store write) | Rejected by total validation; old config stays; loud log; gateway unaffected |
| Provider regional outage | Breakers open in ~seconds; fallbacks serve; auto-recovery via probes |
| All providers down | Fast 503s with `retry-after`; no queue buildup, flat memory |
| Slow-reading client on a stream | Backpressure to upstream via flow control; no unbounded buffering |
| Burst over provider concurrency | Semaphore sheds to fallback/429 immediately; no hidden queueing |
| Gateway crash/replace | Recovers from its local WAL (managed mode) or config file — no external dependency in the recovery path; a replacement cluster node re-syncs by snapshot via `--join` |
