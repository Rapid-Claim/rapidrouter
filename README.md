# rapid-router

**One OpenAI-compatible API in front of every LLM provider — in Rust, with
microseconds of added latency.**

Point any OpenAI SDK at `http://localhost:8080/v1`, set
`"model": "anthropic/claude-sonnet-4-5"`, and rapid-router translates the
request into Anthropic's wire format, streams the response back as OpenAI
chunks, load-balances across your keys, and fails over to a backup provider
when one degrades — from a single static binary that carries its own
storage and web console.

```bash
export OPENAI_API_KEY=sk-... ANTHROPIC_API_KEY=sk-ant-...
rapid-router                       # zero-config start on :8080

curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{"model": "anthropic/claude-sonnet-4-5",
       "messages": [{"role":"user","content":"hi"}]}'
```

Any provider whose conventional environment variable is set is live
immediately. No config file, no database, no sidecar.

---

## Why another gateway

- **One integration, every provider.** Applications write against one API
  shape and switch models — or entire providers — by changing a string.
- **Reliability you don't have to build.** Key rotation, weighted load
  balancing, circuit breakers, and cross-provider fallback chains live in
  the gateway, not in every application.
- **Control of the credential path.** Provider keys stay in the gateway;
  applications hold scoped virtual keys with budgets and rate limits.
- **It gets out of the way.** The overhead is measured per request and
  returned to the caller in a response header — you never have to take our
  word for it.

## What ships

| Area | What ships |
|---|---|
| Unified API | `/v1/chat/completions`, `/v1/responses`, `/v1/embeddings`, `/v1/completions`, `/v1/models` — OpenAI wire format for every provider |
| Drop-in dialects | `/anthropic/*` (Anthropic wire format), `/genai/*` (Google wire format) — official SDKs connect unmodified |
| Cross-dialect routing | Any inbound dialect → any provider (Anthropic SDK code driving an OpenAI model, and the reverse) |
| Streaming | First-class SSE path; per-chunk overhead in nanoseconds on passthrough, ~0.5 µs translated |
| Multimodal | Image / audio / file content parts translated across dialects; large binary payloads handled zero-copy |
| Reliability | Weighted multi-key load balancing, circuit breakers, fallback chains, per-provider backpressure |
| Passthrough | `ANY /passthrough/{provider}/…` — verbatim forward with gateway auth and metering, so new provider features work the day they ship |
| Media relay | `/v1/audio/*`, `/v1/images/generations`, `/v1/files` |
| Config | Single TOML/JSON file or console-managed; `env.*` / `store.*` secret references; atomic hot reload that can never take the gateway down |
| Virtual keys | Scoped gateway credentials with budgets and rate limits — provider keys never leave the gateway |
| Web console | Embedded SPA at `/console` — dashboards, config editing, keys, playground; no separate deployment |
| Fleet mode | N stateless nodes, same binary, sharing one S3 bucket or DynamoDB table — and nothing else |
| Subscription seats | Claude Code and Codex seats as provider keys, benched on the provider's own quota windows ([caveats](docs/components/agent-subscriptions.md)) |
| Observability | Prometheus metrics, structured JSON logs, optional OTLP traces, `x-rapid-overhead-us` receipt header |

## Providers

OpenAI · Azure OpenAI · Anthropic · Google Gemini · AWS Bedrock · Vertex AI ·
Databricks — plus any OpenAI-compatible endpoint by configuration, with
presets for Groq, Mistral, Cerebras, OpenRouter, Ollama, vLLM and friends.

Adding an OpenAI-compatible provider is configuration, not code:

```toml
[providers.groq]
type = "openai_compat"           # preset fills base URL, auth style, quirks
keys = [{ name = "default", value = "env.GROQ_API_KEY" }]
```

See [docs/providers.md](docs/providers.md) for the capability matrix and
per-provider notes.

## Point your SDK at it

```python
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8080/v1", api_key="ck-...")
client.chat.completions.create(
    model="anthropic/claude-sonnet-4-5",
    messages=[{"role": "user", "content": "hello"}],
)
```

```python
from anthropic import Anthropic          # its own dialect, via /anthropic
client = Anthropic(base_url="http://localhost:8080/anthropic", api_key="ck-...")
```

Coding agents are first-class clients — Claude Code via
`ANTHROPIC_BASE_URL=http://localhost:8080/anthropic`, Codex via
`OPENAI_BASE_URL=http://localhost:8080/v1`, and anything OpenAI-compatible
(Cursor, Continue, aider) by base URL alone. An alias re-points an agent at a
different model without the agent knowing:

```toml
[aliases]
"claude-sonnet-4-5" = "openai/gpt-4o"    # Claude Code now drives gpt-4o
```

See [docs/guides/coding-agents.md](docs/guides/coding-agents.md).

## Subscription seats

Claude Code and Codex **subscription** seats can serve traffic as ordinary
provider keys, so a pool of flat-cost seats becomes a bulk capacity tier
with the metered API as its fallback:

```toml
[providers.codex]
type = "codex_subscription"
keys = [{ name = "seat-1", value = "file:/etc/rapid/codex/seat-1/auth.json" }]

[fallbacks]
"codex/gpt-5.5" = ["openai/gpt-4o"]      # overflow to the metered API
```

A rate-limited seat is benched for the window the provider itself reports —
not a guessed cooldown — and a pool with every seat exhausted answers `429`
with a real reset, not `503`. The Claude path is verified live end to end
(text, streaming, tool calls); the Codex path's credential, headers, refresh
and quota handling are, but its success path is not yet.

Read [docs/components/agent-subscriptions.md](docs/components/agent-subscriptions.md)
before turning this on: subscription terms, what is verified live and what
is not, and what is still missing (Claude token renewal, fleet-safe
credential ownership) are all stated there.

## Configure

Everything beyond zero-config — weights, routing groups, keys, budgets — is
editable in the embedded console (set an admin key, open
`http://localhost:8080/console`) or in one file:

```toml
[providers.openai]
keys = [
  { name = "primary", value = "env.OPENAI_API_KEY", weight = 3 },
  { name = "backup",  value = "env.OPENAI_API_KEY_2" },
]

[groups.fast]                             # callers send `fast` as the model
primary = [                               # weights are ratios: 75 / 25
  { target = "groq/llama-3.3-70b-versatile", weight = 3 },
  { target = "openai/gpt-4o-mini",           weight = 1 },
]
fallback = [                              # only once the primary pool is out
  { target = "anthropic/claude-haiku-4-5" },
]
```

A **routing group** is one model id over two weighted pools. Every model in
`primary` serves live traffic in proportion to its weight, so a group is how
you send 75% of a workload to one provider and 25% to another. `fallback` is
a reserve: nothing in it is used while any primary model can still be tried,
and its weights only order the reserve among itself.

Config is validated in full before it is swapped in; an invalid reload keeps
the running table and in-flight requests keep the snapshot they started on.

## Scale

One box works with nothing else installed. N boxes share one external store
— S3, DynamoDB, or a shared file — and nothing else: nodes are stateless,
hold no durable identity, and can be added or removed without coordination.

```bash
rapid-router --store s3 --s3-bucket my-rapid-state --advertise $(hostname)
```

See [docs/operations/fleet.md](docs/operations/fleet.md).

## Performance

From [docs/perf-notes/2026-08-15-baseline.md](docs/perf-notes/2026-08-15-baseline.md)
(Apple Silicon laptop, release profile, in-process mock upstream, 1,000 RPS —
reproduce with `cargo bench` and `cargo run --release -p rig -- overhead`):

| Measure | p50 | p99 |
|---|---|---|
| Gateway-internal overhead (`x-rapid-overhead-us`) | 6 µs | 24 µs |
| Route resolve / key admission | 42 ns / 39 ns | — |
| Anthropic request translation (tool conversation) | 2.9 µs | — |

RSS is flat across a soak run. The budgets (<10 µs p50 passthrough, <20 µs
translated, <100 µs p99) are CI gates, not aspirations — see
[docs/operations/benchmarking.md](docs/operations/benchmarking.md) for the
methodology.

## Build from source

```bash
cargo build --release          # binary at target/release/rapid-router
cargo test --workspace
cargo bench                    # criterion micro benchmarks
cargo run --release -p rig -- overhead --rps 1000 --secs 8
```

The console is a Vite SPA embedded into the binary at build time:

```bash
cd console && npm install && npm run build
```

Useful CLI:

```bash
rapid-router check config.toml     # validate and exit
rapid-router key create --name ci  # issue a virtual key (printed once)
rapid-router fleet                 # which nodes are serving this store
rapid-router master-key            # generate the fleet secret-sealing key
```

## Documentation

[docs/README.md](docs/README.md) is the map. Start with
[docs/01-overview.md](docs/01-overview.md) for what this is and
[docs/guides/quickstart.md](docs/guides/quickstart.md) for five minutes to a
first request; then
[docs/architecture/01-system-design.md](docs/architecture/01-system-design.md)
for how it is put together,
[docs/api/01-endpoints.md](docs/api/01-endpoints.md) for the wire surface, and
[docs/operations/deployment.md](docs/operations/deployment.md) for running it.

[plan.md](plan.md) is the build plan, phase by phase, with the exit criterion
each phase had to clear. [docs/roadmap.md](docs/roadmap.md) is what comes
next.

## Non-goals

Not a Python/JS library — this is a network service. Not a prompt-management,
evals, or observability platform. No training or fine-tuning proxying, and no
telemetry of any kind.

## License

Apache-2.0.
