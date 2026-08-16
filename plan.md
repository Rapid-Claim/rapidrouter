# rapid-router — End-to-End Build Plan

Phased plan for building the gateway described in [docs/](docs/README.md).
Each phase lists what gets built, what gets tested, and a hard exit
criterion — a phase is done when its tests gate CI, not when its code
compiles. Function/tool calling is treated as the highest-risk surface and
gets its own test matrix that grows from Phase 2 onward.

Guiding rules:

1. **Translation is test-first.** Every adapter behavior lands as a golden
   fixture before the code that satisfies it.
2. **Fast paths ship with equivalence proofs** (fuzz targets), or they
   don't ship.
3. **Borrowed tests over invented tests** where possible — LiteLLM,
   provider SDKs, and Bifrost have already encoded years of edge cases;
   porting their suites against our gateway is cheaper and more honest
   than writing our own from scratch (licenses permitting; see §Borrowed
   test corpus).
4. **Every phase keeps `main` releasable.**

---

## Phase 0 — Skeleton & CI backbone (week 1)

**Build**
- Cargo workspace: `router-core`, `router-providers`, `router-server`,
  `router-bin` (+ empty `router-store`, `console/` placeholders).
- Config schema + total validation + `rapid-router check`; `env.*` secret
  references; `SecretString`.
- axum server: `/health`, `/metrics` stub, graceful drain.
- CI: fmt, clippy (deny warnings), test, cargo-audit/deny; release build
  with musl target to prove the static-binary story early.

**Tests**
- Config validation table tests: every invalid-config class → pathed error
  (empty keys, bad weights, alias cycles, unknown fields, missing env).
- `SecretString` redaction tests (Debug/Display/serde-refuse).
- Drain test: SIGTERM with in-flight request completes, new conns refused.

**Exit**: binary boots zero-config, health-checks, drains cleanly; CI green
matrix (linux-gnu, linux-musl, macos).

---

## Phase 1 — Router core (weeks 2–3)

**Build**
- `RoutingTable` + arc-swap snapshot/reload; model resolution
  (prefix/alias/catalog).
- Vose alias-table key selection; per-key atomics (health, in-flight);
  circuit breaker; fallback chain iterator; per-provider semaphores.
- Unified error taxonomy → OpenAI error bodies.

**Tests**
- Property tests (proptest): weighted selection distribution converges to
  weights; masking never selects unhealthy keys; alias resolution total.
- **loom** models: breaker state transitions and token-bucket accounting
  under interleaving — the LiteLLM failure classes (concurrent requests
  bypassing limits, wrong token counting) written as our invariants.
- Reload tests: swap under load, in-flight requests keep old snapshot;
  invalid reload keeps old table.
- Kill-matrix sim: scripted provider-failure sequences → assert breaker
  open/half-open/close timings and fallback order.

**Exit**: 10k-iteration proptest + loom suites green in CI; simulated
multi-key failover works under load test.

---

## Phase 2 — OpenAI passthrough gateway (weeks 3–4)

**Build**
- `/v1/chat/completions` end-to-end against real OpenAI + any
  `openai_compat` endpoint: lazy `model`/`stream` extraction, splice
  rewrite, auth injection, hyper upstream pool, SSE raw forwarding.
- `openai_compat` presets (groq, mistral, ollama, vllm, openrouter).
- `/v1/models`, `/v1/completions`, `/v1/embeddings` relay.
- Receipt headers (`x-rapid-*`), request IDs, metrics.

**Tests — this is where the borrowed suites start**
- **Mock provider** crate (in-repo): OpenAI-shaped server with scripted
  responses/streams/errors/latency — the foundation every later suite
  runs against.
- Splice fuzz target: splice ≡ full-parse-rewrite for arbitrary bodies.
- **OpenAI SDK suites (python + TS) pointed at the gateway** in front of
  the mock: chat, streaming, tools (passthrough), vision parts, JSON
  mode, error handling, timeouts, retries. Run the SDKs' own examples +
  our scripted scenarios in CI.
- SSE codec tests: adversarial chunk boundaries (split mid-`data:`,
  mid-UTF-8, mid-event), `[DONE]` detection, usage chunk.
- **First tool-calling tests (passthrough)**: tools/tool_choice round-trip
  untouched byte-for-byte; streaming tool-call delta frames forwarded
  intact and reassemble correctly in the SDK.

**Exit**: OpenAI python + TS SDK scenario suites green against gateway+mock
in CI; a real `curl` through Groq/OpenAI works; splice fuzzer clean for
24h run.

---

## Phase 3 — Translation: Anthropic + Gemini, tool calling in anger (weeks 5–8)

The hardest phase. Tool calling across dialects is the product's riskiest
surface; it gets a dedicated matrix.

**Build**
- Anthropic adapter: messages/system/content blocks, `max_tokens`
  injection, tool_use ↔ tool_calls, tool_result ↔ tool role, streaming
  event state machine, prompt-caching passthrough.
- Gemini adapter: contents/parts, systemInstruction, functionCall/
  functionResponse, generation config, streaming.
- `/anthropic` + `/genai` inbound dialects (full cross-dialect matrix).
- Capability tables + precise 400s; `dropped_params` accounting.
- Multimodal content parts translation (image/audio/file), opaque-span
  base64 handling.

**Tool/function-calling test matrix** (every cell = golden fixtures +
integration test through the mock; ✓ = must pass to exit):

| Scenario | oai→ant | oai→gem | ant→oai | ant→gem | gem→oai |
|---|---|---|---|---|---|
| single tool call, sync | ✓ | ✓ | ✓ | ✓ | ✓ |
| single tool call, streamed deltas | ✓ | ✓ | ✓ | ✓ | ✓ |
| parallel tool calls | ✓ | ✓ | ✓ | ✓ | ✓ |
| tool_choice: auto/none/required/named | ✓ | ✓ | ✓ | ✓ | ✓ |
| multi-turn: tool result(s) → follow-up | ✓ | ✓ | ✓ | ✓ | ✓ |
| tool + text mixed content in one turn | ✓ | ✓ | ✓ | ✓ | ✓ |
| streamed args split across chunks (incl. mid-UTF-8, mid-escape) | ✓ | ✓ | ✓ | ✓ | ✓ |
| json_schema / strict mode (emulation path where needed) | ✓ | ✓ | ✓ | ✓ | ✓ |
| tool error/rejected result round-trip | ✓ | ✓ | ✓ | ✓ | ✓ |
| id mapping stability across turns | ✓ | ✓ | ✓ | ✓ | ✓ |

**Tests**
- Golden transcript fixtures recorded from real providers (sanitized),
  including full streaming transcripts — fixtures are the adapter spec.
- Translator fuzz targets: round-trip semantic preservation; stream state
  machine never panics/mis-indexes on adversarial event orders.
- **Anthropic SDK suites (python + TS)** against `/anthropic` + mock.
- **Borrowed: LiteLLM translation test cases** (MIT) — port their
  provider-translation unit tests (message shaping, param mapping edge
  cases, tool schemas) as fixture inputs against our adapters.
- Nightly **differential tests**: same request → real provider direct vs
  through gateway → shape-compare (budgeted key spend).
- **Claude Code live scenario**: scripted session (edit file via tools,
  streaming, thinking) through `/anthropic` → non-Anthropic model.

**Exit**: full tool matrix green; Claude Code completes a scripted
multi-tool session against an OpenAI model through the gateway; Anthropic
SDK suites green; translator fuzzers clean 24h.

---

## Phase 4 — Responses API + agent clients (weeks 9–10)

**Build**
- `/v1/responses`: relay (OpenAI/Azure) + stateless translate elsewhere;
  streaming event translation; `store`/`previous_response_id` precise 400
  on translate targets.
- Responses-native tool calling (function tools + built-in tool relay).

**Tests**
- **Codex scripted scenario** through `/v1/responses`.
- OpenAI SDK `responses.*` suites against relay + translate targets.
- Golden fixtures for responses↔chat-completions internal mapping.
- Vercel AI SDK + LangChain smoke suites against `/v1`.

**Exit**: Codex completes a scripted coding session via the gateway;
responses fixtures + SDK suites gate CI.

---

## Phase 5 — Performance proof (weeks 11–12)

**Build**
- Three-tier JSON engine finalized (sonic-rs), buffer reuse, mimalloc,
  PGO release profile.
- Bench rigs: criterion micro (per stage), e2e overhead rig (fixed-RPS vs
  mock, p50/p99/p999 delta), streaming rig (TTFT + per-chunk), 24h soak
  (flat RSS gate).

**Tests**
- CI perf gates: <10 µs p50 passthrough, <20 µs translated, <100 µs p99;
  10 % regression threshold on micro benches; soak RSS flat.
- Published comparison table — run LiteLLM and Bifrost on the same box
  with the same rig (their numbers, our methodology, reproducible).

**Exit**: published numbers meet targets; perf gates + soak in CI;
flamegraph baseline committed.

---

## Phase 6 — Breadth: Azure, Bedrock, passthrough (weeks 13–15)

**Build**
- Azure adapter (deployment mapping); Bedrock adapter (SigV4,
  Converse/ConverseStream event-stream decoding).
- `ANY /passthrough/{provider}/…`; multipart streaming for future audio.
- v1.0 release pipeline: musl + macos binaries, `FROM scratch` image,
  cosign, SBOM.

**Tests**
- Bedrock event-stream codec fuzz (binary framing).
- Tool matrix extended to azure + bedrock columns.
- Passthrough conformance: verbatim forward (headers/body/status),
  auth injection, metering.
- **Release gate = the full named-client matrix**: OpenAI SDK (py/ts),
  Anthropic SDK (py/ts), Claude Code, Codex, Vercel AI SDK, LangChain —
  all green against release candidate.

**Exit**: **v1.0 ships.**

---

## Phase 7 — Managed store, console, virtual keys (v1.x) (weeks 16–20)

**Build**
- Embedded store (single-node document + CAS), `managed`/`file` modes,
  `config export`, `store.*` sealed secrets, usage pipeline + retention.
- Virtual keys end-to-end (hash storage, scopes, budgets, limits,
  rotation grace) per [docs/components/virtual-keys.md](docs/components/virtual-keys.md).
- Console per [docs/components/console.md](docs/components/console.md):
  admin API first (schema-typed), then the 8 pages; rust-embed bundling.
- Audio/images/files relay endpoints.

**Tests**
- Store: single-node crash/recovery (WAL replay), snapshot/restore,
  mode-migration round-trip (`file` ⇄ `managed`).
- Virtual keys: loom/proptest on bucket+budget invariants; scope matrix;
  rotation-grace overlap; revocation propagation latency.
- Console: Playwright e2e per page against seeded gateway; axe (WCAG AA)
  + bundle budget (≤250 KB gz) as CI gates; visual regression on tokens
  (light/dark).
- Admin API: CAS conflict tests; file-mode read-only enforcement.
- Audio relay: multipart streaming without full buffering (memory
  ceiling test with 100 MB upload).

**Exit**: v1.x ships — console demo path (boot binary → browser → add
provider → create virtual key → playground request) runs as an automated
e2e test.

---

## Phase 8 — Stateless fleet (v2) (weeks 21–26)

Originally specced as in-process Raft. Built that way, then replaced:
consensus gave every node a durable identity and a disk that mattered,
which is the opposite of what an autoscaling group, an ECS service, or a
Kubernetes Deployment wants. The control plane is now an external store
with compare-and-swap, and a node holds nothing worth keeping.

**Build**
- `ControlPlane` adapter trait; `file`, `s3`, `dynamodb`, `memory`
  backends. One document, one version, conditional writes.
- Fleet-wide `RAPID_MASTER_KEY` sealing secrets, so any node can read
  what any other node wrote.
- Poll-based propagation and heartbeat-based liveness replacing
  replication and membership; live-N limit shares kept.
- Fleet page, `fleet` and `master-key` CLI, `[store]` config section.

**Tests**
- Backend conformance suite run against all four backends, with S3 and
  DynamoDB exercised over their real wire protocols against in-process
  doubles: round trip, concurrent-writer conflict, concurrent-create
  conflict, full-document fidelity, liveness.
- Store facade: cached reads, operator CAS vs versionless retry,
  cross-node secret round trip, refusal to start unshared.
- Fleet behavior: propagation between nodes, shares tracking liveness,
  serving through a store outage, two operators conflicting, a cold node
  matching a warm one, and a config that fails to build being declined
  rather than taking the node down.
- Two real binaries over one store: propagation, clean-departure share
  reclaim, cold join with no config file.

**Exit**: v2 ships — scaling the node count up or down requires no
coordination, and no node holds state whose loss matters.

---

## Phase 9 — Agent subscriptions (built; partially verified)

Serving traffic from Claude Code and Codex **subscription seats** as
ordinary provider keys, so a pool of flat-cost seats becomes a bulk
capacity tier behind the same router, with the metered API as its
fallback chain. Full design, including the prior art it is drawn from,
in [docs/components/agent-subscriptions.md](docs/components/agent-subscriptions.md).

**Gate — passed (2026-08-16).** A Claude Code OAuth token serves full
Messages traffic: text, streaming, and tool calls all verified live through
the built gateway on `claude-sonnet-4-5`, with the caller's system prompt
still steering the answer. The CLI-transport fallback is not needed.

One measured surprise decided the design: the Claude Code identity block is
**required** on Sonnet — without it the backend answers `429
rate_limit_error` on a fresh quota window — while Haiku serves either way.
The `anthropic-beta` OAuth flag, assumed to be what admits the token,
turns out to change nothing. The Codex loop, including OAuth refresh with
write-back, was verified end to end; its success path was not, for want of
quota.

**Build**
- `Credential` behind `ApiKey`: static + refreshable, published through
  arc-swap; proactive (pre-`exp`) and reactive (401) OAuth refresh,
  single-flight per key, atomic write-back, store-mediated in a fleet.
- Breaker `open_until(deadline)` fed by provider-reported reset windows
  (Codex: header, then body top level **and** nested under `error`;
  Claude: `anthropic-ratelimit-unified-*-reset`), clamped to [1 s, 24 h]
  with 0–10% one-sided jitter.
- `claude_subscription` kind: OAuth bearer + `anthropic-beta`, Claude Code
  identity pinning, existing Anthropic adapter otherwise.
- `codex_subscription` kind: auth.json credentials, Codex CLI header set
  with configurable `Version`, Responses body (`store: false`, no
  `max_output_tokens`), tool calls from `response.output_item.done`.
- Console seat view + per-seat quota metrics; sealed seat credentials in
  the store.

**Tests**
- Loom/proptest on the refreshable-credential swap and deadline breaker
  (no request ever reads a torn credential; a benched seat is never
  admitted early).
- Recorded-transcript mock for both backends, including the empty-`output`
  `response.completed` and the header-less nested 429 — the two silent
  failures that cost AGI Gateway production incidents.
- Existing Anthropic conformance + tool matrix re-run against the
  subscription provider unchanged.
- Fleet: two nodes, one seat, one refresh — exactly one winner, and the
  refresh token survives.

**Exit — met for Claude, partially for Codex.** Built and green: the
credential layer, deadline benching with jitter, both provider kinds,
Codex OAuth renewal with atomic write-back and single-flight, an
end-to-end suite over a mock that reproduces the backend's two
silent-failure shapes, and a live suite that passes against the real
Anthropic backend. **Not met**: Claude token renewal, a live-verified
Codex success path, fleet-safe credential ownership, and amortized seat
cost attribution.
Tracked in [docs/components/agent-subscriptions.md](docs/components/agent-subscriptions.md) §5.

---

## The test pyramid (cumulative, all phases)

| Layer | Tooling | Gates |
|---|---|---|
| Unit + property | cargo test, proptest | every PR |
| Concurrency models | loom | every PR (bounded), nightly (deep) |
| Golden fixtures | in-repo corpus | every PR |
| Fuzzing | cargo-fuzz (splice, translators, SSE, event-stream, error mapper) | PR smoke (60s each), continuous on main |
| SDK matrix | official SDKs via docker-compose + mock provider | every PR |
| Agent scenarios | scripted Claude Code / Codex sessions | nightly + release |
| Differential vs real providers | recorded + live (budgeted) | nightly |
| Performance | criterion + rigs + soak | PR (micro), nightly (rigs), release (soak) |
| Console e2e/a11y | Playwright + axe | every PR touching console |
| Cluster chaos | multi-node harness | nightly from Phase 8 |

## Borrowed test corpus — sources & rules

| Source | License | What we take | How |
|---|---|---|---|
| LiteLLM (BerriAI) | MIT | provider translation unit-test cases, param-mapping edge cases, tool-schema oddities, their documented rate-limit bug reports as regression inputs | port as fixture inputs; keep attribution in `tests/corpus/NOTICE` |
| Bifrost (maximhq) | check license before porting code; behavior is uncopyrightable | integration *scenarios* (drop-in prefix behaviors, fallback semantics), benchmark methodology | reimplement scenarios; don't copy code unless license allows |
| openai-python / openai-node | Apache-2.0 / MIT | run their SDKs' own test-suite-adjacent examples against our base_url; wire-format expectations | run-as-client (no porting needed) + extract streaming fixtures |
| anthropic-sdk-python / -typescript | MIT | same, against `/anthropic` | run-as-client |
| Vercel AI SDK, LangChain | MIT | provider-integration smoke tests pointed at gateway | run-as-client |
| Provider OpenAPI specs (OpenAI, Anthropic) | — | schema conformance: generate validators for request/response shapes | contract tests in CI |

Rules: (1) verify license before vendoring anything; attribution file
mandatory. (2) Prefer *running* foreign suites as clients over porting
their code. (3) Every borrowed case that fails becomes a named regression
test — the corpus only grows.

## Standing risks to watch

- **Streaming tool-call translation** is the likeliest source of subtle
  breakage — hence the matrix, fuzzers, and agent scenarios all hit it.
- **Provider API drift**: nightly differential + SDK-update canary job
  (run matrix against SDK `latest` weekly) catch it before users do.
- **Perf regressions arrive silently**: gates on every PR, flamegraph
  diffs on release.
- **Scope creep from the long tail of endpoints**: passthrough is the
  pressure valve — new provider features route there until demand proves
  a translated endpoint.
