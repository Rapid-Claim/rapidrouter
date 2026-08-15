# Request Lifecycle

The life of `POST /v1/chat/completions`, stage by stage. "Gateway overhead"
means everything caret-router adds on top of the provider's own time; it is
measured per-request and reported in the `x-caret-overhead-us` response
header.

## Stages

```
 ① accept   ② parse    ③ auth+mw   ④ route    ⑤ translate  ⑥ upstream   ⑦ translate  ⑧ respond
 ┌───────┐  ┌────────┐  ┌────────┐  ┌───────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌────────┐
 │ hyper │─►│ sonic  │─►│ tower  │─►│ route │─►│ adapter │─►│ hyper   │─►│ adapter │─►│ write  │
 │ h1/h2 │  │ lazy   │  │ layers │  │ table │  │ req xln │  │ client  │  │ resp xln│  │ body   │
 └───────┘  └────────┘  └────────┘  └───────┘  └─────────┘  └─────────┘  └─────────┘  └────────┘
   ~1µs       ~1-3µs      ~0.2µs      ~50ns      0-5µs       (provider)     0-5µs       ~1µs
```

### ① Accept
hyper reads the request; the body arrives as ref-counted `Bytes`.
Content-Length-bounded, `max_body_size` capped (default 50 MB — inline
base64 images and audio are normal traffic).

### ② Parse — lazily
The gateway does not deserialize the whole body to route it. Routing needs
`model` and `stream`; sonic-rs extracts them by path with a SIMD structural
scan — no DOM, no allocation. Full typed parsing happens only if the target
dialect requires translation ([04-json-engine.md](04-json-engine.md)).

### ③ Middleware
Compiled tower layers: request-ID stamping, gateway auth (constant-time key
compare), metrics start-timestamp, then registered `Hook::pre` callbacks
([../components/hooks.md](../components/hooks.md)).

### ④ Route
Load the routing snapshot (`ArcSwap<RoutingTable>`, lock-free, ~1 ns).
Resolve `provider/model` prefix, alias, or catalog entry. Pick a key by
weighted alias-table sampling over healthy keys (O(1)). Check the circuit
breaker (one atomic load). Acquire the provider's concurrency permit. Build
the fallback iterator.

### ⑤ Translate request
Same-dialect targets: splice the `model` bytes in place, set auth — the
messages array is never materialized. Foreign dialects: one-pass borrowed
translation into a pre-sized buffer.

### ⑥ Upstream call
Per-provider hyper pool, HTTP/2 where supported, rustls session resumption.
Timeouts: connect (2 s), time-to-first-byte, total. Retryable failures
(connect error, 429, 5xx before any byte) advance the chain: next key, then
next fallback provider — re-translating dialect if the chain crosses one.

### ⑦ Translate response
Non-streaming: single-pass translation to the inbound dialect's shape.
Streaming: per-event translation as frames arrive
([../api/03-streaming.md](../api/03-streaming.md)); same-dialect streams
forward raw frames.

### ⑧ Respond
Write status, headers (`x-request-id`, `x-caret-provider`, `x-caret-model`,
`x-caret-overhead-us`), body or stream. Record histograms, release the
permit, update breaker state, run `Hook::post`.

## Latency budgets (p50, enforced by CI benches)

| Stage | Budget |
|---|---|
| HTTP parse + body assembly | 1 µs |
| Lazy field extraction | 1–3 µs (2 KB body) |
| Middleware + hooks (empty set) | 200 ns |
| Routing + key pick + breaker + permit | < 100 ns |
| Request translation | 0 (splice) – 5 µs (full, 2 KB) |
| Response translation | 0 – 5 µs; ~0.5 µs per streamed chunk |
| **Total added overhead** | **< 10 µs passthrough · < 20 µs translated** |

## The error path

Every failure — malformed body, unknown model, no healthy key, breaker open,
saturated provider, upstream error, timeout — maps through one taxonomy to
the *inbound dialect's* native error shape with the correct HTTP status
([../api/04-errors.md](../api/04-errors.md)), so SDK retry logic behaves
exactly as it does against the provider directly. Provider detail is
preserved for logs and never leaks key material.
