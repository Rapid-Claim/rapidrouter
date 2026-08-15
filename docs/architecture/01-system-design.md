# System Design

## The system at a glance

```
                        ┌──────────────────────────────────────────────────┐
                        │                caret-router                      │
                        │                                                  │
  OpenAI SDK ──────►    │  ┌─────────┐  ┌──────────┐  ┌─────────────────┐  │
  Anthropic SDK ───►    │  │  HTTP   │─►│Middleware│─►│     Router      │  │
  Google SDK ──────►    │  │ (axum/  │  │ (tower   │  │ model→provider  │  │
  Claude Code ─────►    │  │ hyper)  │  │  layers  │  │ key selection   │  │
  Codex / curl ────►    │  └─────────┘  │ + hooks) │  │ fallback chain  │  │
                        │               └──────────┘  └────────┬────────┘  │
                        │                                      ▼           │
                        │  ┌────────────────────────────────────────────┐  │
                        │  │            Provider Adapters               │  │
                        │  │  openai │ anthropic │ gemini │ azure │     │  │
                        │  │  bedrock │ openai_compat (Groq, Ollama, …) │  │
                        │  │  (request / response / stream translation) │  │
                        │  └────────────────────┬───────────────────────┘  │
                        │                       ▼                          │
                        │  ┌────────────────────────────────────────────┐  │
                        │  │  Upstream client: hyper pools, HTTP/2,     │  │
                        │  │  rustls (session resumption), SigV4        │  │
                        │  └────────────────────────────────────────────┘  │
                        └──────────────────────────────────────────────────┘
                                   │              │              │
                                   ▼              ▼              ▼
                            api.openai.com  api.anthropic.com  …providers
```

Not shown above: the **control plane** — the embedded replicated store
(`router-store`, port 9444) that persists config, virtual keys, and
sealed secrets, sharing them across nodes through an external store. It feeds
the routing table but sits entirely off the request path
([06-state-and-storage.md](06-state-and-storage.md)).

## Crate layout (Cargo workspace)

```
caret-router/
├── Cargo.toml                 # workspace
├── crates/
│   ├── router-core/           # no HTTP server; the embeddable heart
│   │   └── src/
│   │       ├── types/         # unified request/response types (OpenAI-shaped, borrow-friendly)
│   │       ├── router.rs      # model resolution, key selection, fallbacks, breakers
│   │       ├── config.rs      # schema, validation, hot-reload plumbing
│   │       ├── secret.rs      # SecretString (redacted, zeroized)
│   │       └── error.rs       # unified error taxonomy
│   ├── router-providers/      # Provider trait + one adapter per dialect
│   ├── router-server/         # axum surface: routes, dialects, SSE, middleware, hooks
│   ├── router-store/        # control plane: backend adapters (file, S3, DynamoDB), sealing
│   └── router-bin/            # main.rs: CLI, config load, runtime setup
├── benches/                   # criterion micro-benches + e2e overhead rigs
├── fuzz/                      # cargo-fuzz targets (JSON splice, translators, SSE codec)
└── docs/
```

`router-core` + `router-providers` have no server dependency — they are what
benchmarks, fuzzers, and embedders target. `router-server` is thin by design.

## Technology stack

| Concern | Choice | Rationale |
|---|---|---|
| Async runtime | tokio, multi-threaded work-stealing | The ecosystem standard; every dependency composes with it |
| HTTP server | axum on hyper 1.x | hyper is the fastest mainstream HTTP stack; axum handlers monomorphize to statically-dispatched tower services with negligible overhead, and native WebSocket support covers the realtime roadmap |
| Upstream client | hyper client (thin in-house wrapper) + rustls | Full control over pooling, HTTP/2, timeouts; no intermediate client framework on the hot path |
| TLS | rustls, session resumption on | No C library linkage; static-binary friendly |
| JSON | sonic-rs (SIMD) on the hot path; serde_json on cold paths | See [04-json-engine.md](04-json-engine.md) |
| Buffers | bytes::Bytes / BytesMut | Ref-counted zero-copy slices; bodies move as pointer arithmetic |
| Shared hot state | arc-swap + atomics | Lock-free reads of routing table and health state on every request |
| Allocator | mimalloc | Measurably better tail latency than the system allocator under our benches |
| Metrics / tracing | metrics + prometheus exporter; tracing (+ optional OTLP) | Atomic-cheap recording; zero cost when disabled |
| AWS auth | aws-sigv4 | Bedrock request signing |
| Control plane | S3 / DynamoDB conditional writes | Compare-and-swap on one document; no consensus, no node state ([06-state-and-storage.md](06-state-and-storage.md)) |

## Design principles

1. **The internal type is the OpenAI wire shape.** There is no third
   "neutral" schema. OpenAI-in → OpenAI-out traffic is near-passthrough;
   only genuinely foreign dialects pay for translation, exactly once, at the
   edge.
2. **No queues on the request path.** A request is one tokio task from
   accept to response. Queues and channel hops add latency and hide
   overload; backpressure is explicit (semaphores) and overload is shed
   honestly (fallback or 429).
3. **No locks on the hot path.** Config snapshots via arc-swap; breaker and
   health state in atomics; key selection from precomputed alias tables.
4. **Zero-copy by default.** Bodies are `Bytes` ropes. A 10 MB base64 image
   crosses the gateway without being copied or decoded.
5. **Fast paths never fork correctness.** Every shortcut (buffer splicing,
   raw stream forwarding) has a fuzz target proving equivalence with the
   straightforward path.
6. **Measured, not asserted.** Overhead budgets are enforced by CI
   benchmarks; the per-request overhead is returned to callers in
   `x-caret-overhead-us`.
7. **The data plane never depends on the control plane.** Serving traffic
   requires only the local applied snapshot — consensus, disk, and the
   console can all be degraded while requests keep flowing.
