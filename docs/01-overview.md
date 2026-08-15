# Overview

caret-router is a network gateway that sits between your applications and
LLM providers. It presents one stable, OpenAI-compatible API; behind it, it
routes, translates, load-balances, meters, and protects.

## Why a gateway

- **One integration, every provider.** Applications write against one API
  shape and switch models — or entire providers — by changing a string.
- **Reliability you don't have to build.** Key rotation, weighted load
  balancing, circuit breakers, and cross-provider fallback chains live in
  the gateway, not in every application.
- **Control of the credential path.** Provider keys stay in the gateway;
  applications hold gateway keys with scoped permissions and budgets.
- **One place to observe.** Latency, token usage, cost, and error rates for
  all LLM traffic, on one Prometheus endpoint.

## Design goals

1. **Latency.** Single-digit microseconds of added overhead p50 on the
   passthrough path, < 20 µs translated; < 100 µs p99. No garbage collector,
   no stop-the-world pauses, no accidental copies. The overhead is measured
   per-request and returned to the caller in a response header.
2. **Compatibility.** Byte-faithful API surfaces. Existing SDKs — and
   existing agents like Claude Code and Codex — work by changing only a base
   URL. Where a provider's real behavior differs from its documentation, we
   match the behavior: SDKs are written against reality.
3. **Correctness under concurrency.** Rate limits that hold when requests
   race. Token accounting that reconciles with provider billing. These are
   verified properties (loom, proptest), not intentions.
4. **Operational simplicity.** One static binary that is the whole system —
   gateway, console, and storage. One box works with nothing else
   installed; N boxes share one S3 bucket or DynamoDB table and nothing else.
   Config changes can never take the gateway down.
5. **Trust.** The gateway holds every provider key you own. Minimal audited
   dependencies, signed releases, SBOM, no telemetry.

## Feature summary

| Area | What ships |
|---|---|
| Unified API | `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/completions`, `/v1/models` — OpenAI wire format for every provider |
| Drop-in dialects | `/anthropic/*` (Anthropic wire format), `/genai/*` (Google wire format) — official SDKs connect unmodified |
| Cross-dialect routing | Any inbound dialect → any provider (Anthropic SDK code driving an OpenAI model, and vice versa) |
| Multimodal | Image / audio / file content parts translated across dialects; large binary payloads handled zero-copy |
| Streaming | First-class SSE path; per-chunk overhead in nanoseconds on passthrough, ~0.5 µs translated |
| Providers | OpenAI, Anthropic, Google Gemini, Azure OpenAI, AWS Bedrock, Vertex AI, Databricks, plus any OpenAI-compatible endpoint by config (Groq, Mistral, Ollama, vLLM, and other presets) |
| Reliability | Weighted multi-key load balancing, circuit breakers, fallback chains, per-provider backpressure |
| Passthrough | `ANY /passthrough/{provider}/…` — verbatim forward with gateway auth and metering; new provider features work the day they ship |
| Config | Single TOML/JSON file or console-managed; `env.*`/`store.*` secret references; atomic hot reload |
| Virtual keys | Scoped gateway credentials with budgets and rate limits — provider keys never leave the gateway ([components/virtual-keys.md](components/virtual-keys.md)) |
| Web console | Embedded single-page app at `/console` — dashboards, config editing, keys, playground; no separate deployment ([components/console.md](components/console.md)) |
| Fleet mode | N stateless nodes, same binary, sharing one external store — S3 or DynamoDB ([operations/fleet.md](operations/fleet.md)) |
| Observability | Prometheus metrics, structured JSON logs, optional OTLP traces, `x-caret-overhead-us` receipt header |

## Non-goals

- Not a Python/JS library or SDK wrapper — caret-router is a network service.
- Not a prompt-management, evals, or observability platform — the embedded
  console covers gateway operations, nothing more.
- No training or fine-tuning proxying.
- No video-generation translation (the passthrough route covers those APIs).

## Status

See [roadmap.md](roadmap.md) for the release plan: the v1.0 surface above;
then the managed config store, web console, governance (virtual keys,
budgets), and audio endpoints; then the stateless fleet model and realtime
audio-to-audio.
