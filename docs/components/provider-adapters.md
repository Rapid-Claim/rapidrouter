# Provider Adapters

`router-providers` contains one adapter per upstream dialect. An adapter
answers four questions: how to build the wire request, how to authenticate,
how to translate the response, and how to translate the stream.

## The trait

```rust
pub trait Provider: Send + Sync + 'static {
    fn id(&self) -> ProviderId;
    fn capabilities(&self) -> &Capabilities;   // drives the published matrix

    /// Build the upstream request; may splice the inbound buffer (same
    /// dialect) or fully translate it (foreign dialect).
    fn build_request(
        &self,
        route: &ResolvedRoute,          // model, key, base_url, deployment
        inbound: InboundDialect,        // OpenAI | Anthropic | Gemini
        body: &Bytes,
    ) -> Result<UpstreamRequest, AdapterError>;

    /// Translate a complete (non-streaming) response body.
    fn translate_response(
        &self, outbound: OutboundDialect, body: &Bytes,
    ) -> Result<Bytes, AdapterError>;

    /// Wrap the upstream byte stream into translated events.
    fn translate_stream(
        &self, outbound: OutboundDialect, upstream: BodyStream,
    ) -> TranslatedStream;

    /// Map provider failures into the unified taxonomy (drives breakers
    /// and client-facing error shapes).
    fn classify_error(&self, status: StatusCode, body: &Bytes) -> GatewayError;
}
```

Adapters are stateless and shared (`Arc<dyn Provider>` in the routing
table); per-request state lives on the stack. One vtable call per stage —
nanoseconds, irrelevant beside translation work.

## The adapter set

| Adapter | Dialect notes |
|---|---|
| `openai` | Native dialect; splice path; base for `openai_compat` |
| `openai_compat` | Generic OpenAI-compatible upstreams — Groq, Mistral, Ollama, Cerebras, vLLM, OpenRouter, any custom `base_url`. Per-preset quirk table declares parameters to strip and auth style; adding a provider here is **configuration, not code** |
| `anthropic` | Messages API: system prompt relocation, content blocks, required `max_tokens` (default injected), tool_use/tool_result ↔ tool_calls, cache-control passthrough, `x-api-key` + version headers |
| `gemini` | `generateContent`/`streamGenerateContent`: contents/parts, role mapping, `systemInstruction`, generation-config mapping, function-calling parts |
| `azure` | OpenAI dialect over Azure's URL scheme (`{resource}/openai/deployments/{deployment}` + `api-version`), `api-key` header, per-model deployment map from config |
| `bedrock` | Converse/ConverseStream APIs, SigV4 signing, AWS event-stream framing |

## Translation rules

1. **One pass, no DOM** ([../architecture/04-json-engine.md](../architecture/04-json-engine.md)).
2. **Preserve what fits, drop loudly what doesn't**: parameters the target
   can't express are stripped with a `dropped_params` metric and debug log —
   never a silent behavior change, never a 500.
3. **Capabilities are declared, not discovered**: each adapter's
   `Capabilities` table (tools, vision, audio parts, documents, JSON mode,
   logprobs, streaming) drives precise `400`s and the published matrix —
   crucial for fallback chains that cross dialects.
4. **Tool calling is the hard 20 %**: id conventions, argument encodings,
   parallel-call semantics, and each dialect's streaming-delta accumulation
   rules get dedicated translators and the densest test coverage.
5. **Golden fixtures are the spec**: every adapter ships request/response/
   stream-transcript fixture pairs; CI diffs actual against golden, and the
   fixtures double as executable documentation.

## Authentication

Adapters receive a resolved `AuthSpec`, never raw strings:

| Style | Used by |
|---|---|
| `Bearer` | OpenAI, Groq, Mistral, OpenRouter, … |
| `Header(name)` | Anthropic (`x-api-key`), Azure (`api-key`) |
| `QueryParam` | Gemini (`?key=`) |
| `SigV4 { region, service }` | Bedrock |
| `None` | Local endpoints (Ollama, vLLM) |

Key material stays inside `SecretString`
([../architecture/05-security.md](../architecture/05-security.md)).
