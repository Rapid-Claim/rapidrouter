# Roadmap

## v1.0 — the core gateway

The surface documented throughout these docs:

- `/v1/chat/completions` (+ streaming + multimodal parts), `/v1/responses`
  (relay + stateless translate), `/v1/embeddings`, `/v1/completions`,
  `/v1/models`, passthrough, `/health`, `/metrics`.
- Inbound dialects: `/v1`, `/anthropic`, `/genai`; named-client CI (Claude
  Code, Codex, major SDKs).
- Providers: OpenAI, Anthropic, Gemini, Azure, Bedrock + `openai_compat`
  presets.
- Router: weighted keys, breakers, fallbacks, semaphore backpressure —
  loom/proptest-verified.
- Config in `file` mode + hot reload; Prometheus; structured logs;
  receipt headers. (The embedded store ships single-node in v1.x with the
  console; its on-disk format is designed in from v1.0 so upgrading is
  additive.)
- Published, reproducible overhead numbers and a flat 24-hour soak.
- Supply-chain posture: static binary, SBOM, signed releases, audited deps.

## v1.x — governance & media endpoints

- **Virtual keys** ([components/virtual-keys.md](components/virtual-keys.md)):
  scoped keys with budgets and rate limits, stored as hashes in the
  embedded store. Built on the Hook seam; correctness-under-concurrency
  bar applies.
- **Spend tracking** export; per-key accounting.
- **Managed config mode**: the embedded store (single-node), console/CLI
  editing, `store.*` secrets sealed at rest, `config export`
  ([architecture/06-state-and-storage.md](architecture/06-state-and-storage.md)).
- **Web console**: embedded SPA + admin API
  ([components/console.md](components/console.md)) — bundled into the
  binary behind the default-on `console` feature; off at runtime until
  admin keys are configured.
- **Usage pipeline**: local JSONL partitions + retention; optional external
  sinks.
- `/v1/audio/speech`, `/v1/audio/transcriptions` (relay), streaming-safe
  multipart.
- `/v1/images/generations`, `/v1/files` (relay).

## v2 — realtime & cluster

- `/v1/realtime`: WebSocket proxy mode + WebRTC ephemeral-token mode
  (audio bypasses the gateway; governance stays), per-session metering.
- **Cluster mode**: multi-node replication of the managed store (Raft),
  join tokens, peer scatter-gather for fleet views, live-N rate-limit
  shares ([operations/clustering.md](operations/clustering.md)).
- Gateway-side Responses statefulness (`store`, `previous_response_id` for
  translated targets) — persisted in the replicated store; decided
  deliberately.
- Idle-share quota rebalancing across cluster members (strict global
  limits stay permanently out of scope — consensus never belongs on the
  hot path).

## Exploratory

- Sandboxed plugin runtime (WASM) at the Hook seam.
- Semantic/response caching as a plugin.
- MCP tool injection.
- Cost/latency-aware model routing policies.
- io_uring-backed runtime experiment.

## Permanent non-goals

Python/JS library packaging, prompt management, evals platforms,
observability UIs, training/fine-tuning proxying, video-generation
translation (passthrough covers those APIs).
