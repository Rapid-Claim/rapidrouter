# API Surface

Every endpoint is served in one of three modes — the honest vocabulary for
what a gateway can do with a request:

- **Translate** — full dialect translation between wire formats (the
  expensive, valuable kind).
- **Relay** — same wire format on both sides; the gateway routes,
  authenticates, meters, and forwards bytes.
- **Passthrough** — verbatim forward of an arbitrary path to a chosen
  provider, with gateway auth injected and the request metered.

The generated **capability matrix** (endpoint × provider → `translate |
relay | reject`) is published with each release; "do you support X?" always
has a versioned, precise answer.

## Core endpoints (v1.0)

| Endpoint | Mode | Notes |
|---|---|---|
| `POST /v1/chat/completions` | translate | Full schema incl. streaming, tools, multimodal content parts |
| `POST /v1/responses` | relay to OpenAI/Azure; translate (stateless) elsewhere | See below |
| `POST /v1/embeddings` | translate/relay | |
| `POST /v1/completions` | translate/relay | Legacy text completions |
| `GET  /v1/models` | local | Catalog assembled from config: models, routing groups, aliases, capabilities |
| `ANY  /passthrough/{provider}/{path…}` | passthrough | The escape hatch — new provider features work the day they ship |
| `GET  /health`, `GET /metrics` | local | Liveness · Prometheus |
| `/console`, `/admin/api/*` | local | Embedded web console + admin API — present only when admin keys are configured ([../components/console.md](../components/console.md)) |

Drop-in dialect prefixes (`/anthropic/v1/*`, `/genai/*`) expose the same
routing through other wire formats — see [02-dialects.md](02-dialects.md).

## Multimodal inputs are part of chat, and they are core

Files, images, and audio arrive as **content parts inside the messages
array**, not as separate APIs:

| Content part | Handling |
|---|---|
| `image_url` (https or base64 `data:` URI) | Translated across dialects (OpenAI part ↔ Anthropic image block ↔ Gemini inline/file data). Base64 payloads move as opaque byte spans — never decoded, never copied |
| `input_audio` | Same opaque-span treatment; capability-gated per target |
| `file` / document parts (e.g. PDFs) | Translated where both dialects support documents; precise `400` otherwise |
| video inputs | Capability-gated relay to providers that accept them; not cross-translated |

This is why `max_body_size` defaults to 100 MB and why the zero-copy body
design ([../architecture/04-json-engine.md](../architecture/04-json-engine.md))
is a core requirement rather than an optimization.

## The Responses API

`/v1/responses` is a first-class surface:

- **Relay mode** (OpenAI, Azure): the complete surface — reasoning items,
  built-in tools, background mode — forwarded faithfully from day one.
- **Translate mode** (other providers): the stateless core — input items,
  instructions, tools, streaming events — mapped to the target dialect.
- **Statefulness** (`store`, `previous_response_id`): native in relay mode
  (the provider holds the state); rejected with a precise `400` in translate
  mode. Gateway-side conversation state is a deliberate roadmap decision,
  not an accident ([../roadmap.md](../roadmap.md)).

## v1.x endpoints (relay-first)

| Endpoint | Notes |
|---|---|
| `POST /v1/audio/speech` | TTS; streamed audio out is chunked bytes — no JSON on the path |
| `POST /v1/audio/transcriptions` | STT; multipart uploads stream to the provider, never fully buffered |
| `POST /v1/images/generations` | Relay to OpenAI-compatible image APIs first |
| `/v1/files` | Upload/list/retrieve, relayed to the routed provider |

## Realtime (roadmap)

`GET /v1/realtime` (WebSocket upgrade) ships in two modes:

1. **WebSocket proxy** — client ↔ gateway ↔ provider; frames forwarded at
   byte level (audio deltas are never JSON-parsed), sessions metered.
2. **WebRTC token mode** — the gateway issues ephemeral provider tokens and
   relays SDP; **audio flows client ↔ provider directly**, adding zero audio
   latency while the gateway keeps authentication and metering.

The HTTP layer's WebSocket support and the per-session metering model are
reserved in the architecture now so realtime is an addition, not a retrofit.
