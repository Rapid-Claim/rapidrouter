# The Router

`router-core` resolves *what model → which provider → which key*, and owns
reliability: load balancing, fallback, circuit breaking, backpressure.

## The routing snapshot

All routing state is an immutable snapshot behind `ArcSwap`:

```rust
pub struct RoutingTable {
    providers: HashMap<ProviderId, ProviderRuntime>, // adapter, keys, semaphore, breaker state
    catalog:   HashMap<ModelName, ProviderId>,       // bare model name → provider
    aliases:   HashMap<String, TargetModel>,         // "fast" → groq/llama-3.3-70b
    fallbacks: HashMap<TargetModel, Vec<TargetModel>>,
}
// per request: state.table.load()  — lock-free, ~1 ns
```

Any config change — a file reload in `file` mode, a committed
replicated-store write in `managed` mode — builds a complete new table,
validates it, and swaps it atomically. In-flight requests finish on their
old snapshot; a failed validation keeps the old table — a change can never
take the gateway down.

## Model resolution

1. Explicit `provider/model` prefix wins.
2. Alias lookup (config-defined names, repointable without code changes).
3. Catalog lookup for bare names (first configured provider whose key
   allowlist includes the model).
4. Otherwise `404` naming the model and the configured candidates.

## Key selection — O(1), weighted, health-aware

Providers hold N keys with weights. At table build, keys become a **Vose
alias table**: selection is two array reads and one RNG call, constant time
at any key count. Unhealthy keys — breaker-open or over their token budget —
are masked out; if masking empties the table the router falls through to
fallbacks.

Per-key state is a handful of atomics: in-flight count, consecutive-failure
window, last-failure timestamp, optional TPM/RPM token buckets. Token
buckets count **input + output tokens** and exclude provider-cached tokens —
and the invariants (no bypass under concurrent racing, no double-count) are
loom/proptest-verified.

## Circuit breaker

Per (provider, key), three states, all transitions on atomics:

| State | Behavior |
|---|---|
| Closed | Normal; qualifying failures increment a windowed counter |
| Open | After `failure_threshold` (default 5) in `window` (30 s): reject in one atomic load, advance to next candidate |
| Half-open | After `cooldown` (15 s): admit one probe; success closes, failure re-opens |

Qualifying: connect errors, timeouts, 5xx, 429 (which also feeds the key's
rate mask so a throttled key rests). Client-caused 4xx never trips a
breaker.

## Fallback chains

Configured per model or alias:

```toml
[fallbacks]
"openai/gpt-4o" = ["azure/gpt-4o", "anthropic/claude-sonnet-4-5"]
```

Order of attempts: remaining keys of the current provider, then each
fallback target in order — including cross-dialect targets, where the
adapter layer re-translates the request transparently. Only safe failures
advance the chain: connect errors, 429s, and 5xx *before any response byte*.
Served-by is always disclosed via `x-rapid-provider` / `x-rapid-model`.

## Backpressure

A per-provider `Semaphore` bounds in-flight upstream calls. Acquisition
happens inside routing; saturation skips to fallbacks or returns an honest
429 + `retry-after`. There is deliberately **no internal queue** — queues
hide overload and destroy tail latency; shedding is visible and immediate.
