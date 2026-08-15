# Inbound Dialects & Cross-Dialect Routing

caret-router speaks three wire formats *inbound*, selected by URL prefix.
Routing, reliability, and metering are identical behind all three; the
prefix only sets which dialect the client is speaking — and therefore which
shapes it gets back.

| Prefix | Wire format | Typical clients |
|---|---|---|
| `/v1/*` | OpenAI (Chat Completions + Responses) | OpenAI SDKs, Codex, Vercel AI SDK, LangChain, most tools |
| `/anthropic/v1/*` | Anthropic Messages | Anthropic SDKs, **Claude Code** (`ANTHROPIC_BASE_URL`) |
| `/genai/*` | Google GenAI | Google GenAI SDKs |

Adopting the gateway is a base-URL change, whichever SDK you already use.

## Cross-dialect routing is first-class

The inbound dialect and the outbound provider are independent: Anthropic-
dialect code can drive an OpenAI model, OpenAI-dialect code can drive a
Gemini model. This is how you A/B providers — or migrate — without touching
application code.

```
 inbound ↓ / outbound →   openai    anthropic    gemini    openai_compat
 /v1 (openai)              relay     translate   translate     relay
 /anthropic                translate  relay      translate   translate
 /genai                    translate translate     relay     translate
```

Every cell either works or fails with a precise `400` naming the unsupported
feature — driven by the adapters' capability tables, never discovered as a
silent behavior change.

## Model naming

1. `"model": "anthropic/claude-sonnet-4-5"` — explicit `provider/model`.
2. `"model": "gpt-4o-mini"` — bare name resolved through the config catalog.
3. `"model": "fast"` — a config-defined alias (`fast = "groq/llama-3.3-70b"`),
   repointable in config without any application change.

## Dialect fidelity commitments

- **`/v1`**: chunk objects byte-shape-faithful to the OpenAI format
  (including field order where SDKs are sloppy about it), `[DONE]` sentinel,
  `stream_options.include_usage` semantics, OpenAI error bodies and status
  codes so SDK retry/backoff behaves identically.
- **`/anthropic`**: the full Messages event sequence (`message_start →
  content_block_start → content_block_delta → … → message_stop`), tool_use /
  tool_result turns, **prompt-caching headers and beta flags passed
  through** — agent clients depend on cache hits for cost and latency.
- **`/genai`**: `generateContent` / `streamGenerateContent` shapes,
  `systemInstruction`, function-calling parts.

Where a provider's real API differs from its documentation, caret-router
matches the API. Compatibility beats purity.

## Named-client guarantee

Agents exercise dialects harder than SDKs: streaming tool use, parallel
calls, thinking blocks, prompt caching, very long sessions. These clients
are scripted CI scenarios, run against every release:

| Client | Exercises | Via |
|---|---|---|
| Claude Code | streaming tool_use, thinking, cache_control, long contexts | `/anthropic` |
| Codex | the Responses API surface | `/v1/responses` |
| OpenAI SDK (py/ts) | sync, stream, tools, vision, JSON mode, errors | `/v1` |
| Anthropic SDK (py/ts) | messages, event stream, tool_result turns | `/anthropic` |
| Vercel AI SDK, LangChain | framework-level quirks | `/v1` |

See [../guides/coding-agents.md](../guides/coding-agents.md) for setup.
