# Providers

## Supported providers

| Provider | Adapter | Auth | Notes |
|---|---|---|---|
| OpenAI | `openai` | Bearer | Native dialect; splice fast path; Responses API relayed in full |
| Azure OpenAI | `azure` | `api-key` header | Deployment mapping + `api-version` from config; Responses relay |
| Anthropic | `anthropic` | `x-api-key` | Messages API; prompt-caching passthrough; thinking blocks |
| Google Gemini | `gemini` | query/header key | generateContent; video input capable |
| AWS Bedrock | `bedrock` | SigV4 | Converse APIs; event-stream decoding; static/env/profile credentials |
| Groq | `openai_compat` preset | Bearer | |
| Mistral | `openai_compat` preset | Bearer | |
| Cerebras | `openai_compat` preset | Bearer | |
| OpenRouter | `openai_compat` preset | Bearer | |
| Ollama | `openai_compat` preset | none | Local; `base_url` defaulted to `localhost:11434/v1` |
| vLLM / SGLang / any OpenAI-compatible server | `openai_compat` | configurable | `type = "openai_compat"` + `base_url` — configuration, not code |

Presets pre-fill base URL, auth style, and parameter quirks (fields to strip,
streaming idiosyncrasies); every preset value is overridable in config.

## Capability matrix

The authoritative matrix is **generated from the adapters' capability
tables** at build time and published with each release, so support claims
are versioned and exact. Summary of the current tables:

| Capability | openai | azure | anthropic | gemini | bedrock | openai_compat |
|---|---|---|---|---|---|---|
| Chat + streaming | ✓ | ✓ | ✓ | ✓ | ✓ | ✓ |
| Tools / function calling | ✓ | ✓ | ✓ | ✓ | ✓ | preset-dependent |
| Parallel tool calls | ✓ | ✓ | ✓ | ✓ | ✓ | preset-dependent |
| Vision (image parts) | ✓ | ✓ | ✓ | ✓ | ✓ | preset-dependent |
| Audio input parts | ✓ | ✓ | — | ✓ | — | preset-dependent |
| Document/PDF parts | ✓ | ✓ | ✓ | ✓ | ✓ | — |
| Video input | — | — | — | ✓ | — | — |
| JSON mode / json_schema | ✓ | ✓ | emulated¹ | ✓ | model-dependent | preset-dependent |
| logprobs | ✓ | ✓ | — | — | — | preset-dependent |
| `n > 1` | ✓ | ✓ | — | ✓ | — | preset-dependent |
| Responses API | relay | relay | translate² | translate² | translate² | translate² |
| Embeddings | ✓ | ✓ | — | ✓ | ✓ | preset-dependent |
| Prompt caching | ✓ | ✓ | ✓ (passthrough control) | ✓ | ✓ | — |

¹ Emulated via forced tool use; disclosed with `x-caret-emulated: json_schema`.
² Stateless core; `store`/`previous_response_id` rejected with a precise 400.

Unsupported combinations fail at routing time with a `400` naming the
capability — never silently degraded, which matters doubly for fallback
chains that cross providers.

## Adding a provider

- **OpenAI-compatible?** Config only:

  ```toml
  [providers.myserver]
  type = "openai_compat"
  base_url = "https://llm.internal.example/v1"
  keys = [{ name = "main", value = "env.MYSERVER_KEY", weight = 1.0 }]
  ```

- **Foreign dialect?** Implement the `Provider` trait
  ([components/provider-adapters.md](components/provider-adapters.md)) with
  golden fixtures and stream transcripts; the capability table and fuzz
  targets are part of the definition of done.
