# Configuration

Configuration is one document — TOML canonical, JSON accepted with the
identical schema — writable two ways: as a file you edit (`file` mode), or
through the embedded console/CLI, persisted and replicated by the built-in
store (`managed` mode, the default). Same schema either way; no external
database or config service in either mode.

## Example `caret-router.toml`

```toml
[server]
host = "0.0.0.0"
port = 8080
max_body_size_mb = 50
auth_keys = ["env.CARET_GATEWAY_KEY"]     # omit for open localhost use
drain_timeout_secs = 30

[providers.openai]
keys = [
  { name = "primary",   value = "env.OPENAI_API_KEY",   weight = 0.7, models = ["gpt-4o", "gpt-4o-mini"] },
  { name = "secondary", value = "env.OPENAI_API_KEY_2", weight = 0.3 },
]
max_concurrency = 512
timeout_secs = 120

[providers.anthropic]
keys = [{ name = "main", value = "env.ANTHROPIC_API_KEY", weight = 1.0 }]

[providers.groq]                # preset: base_url + quirks pre-filled
type = "openai_compat"
keys = [{ name = "main", value = "env.GROQ_API_KEY", weight = 1.0 }]

[providers.ollama]
type = "openai_compat"
base_url = "http://localhost:11434/v1"
auth = "none"

[providers.azure]
keys = [{ name = "main", value = "env.AZURE_OPENAI_KEY", weight = 1.0 }]
endpoint = "https://myresource.openai.azure.com"
api_version = "2024-10-21"
[providers.azure.deployments]
"gpt-4o" = "my-gpt4o-deployment"

[aliases]
fast  = "groq/llama-3.3-70b-versatile"
smart = "anthropic/claude-sonnet-4-5"

[fallbacks]
"openai/gpt-4o" = ["azure/gpt-4o", "anthropic/claude-sonnet-4-5"]

[reliability.breaker]
failure_threshold = 5
window_secs = 30
cooldown_secs = 15

[reliability.retries]
max_attempts = 2
on = ["connect_error", "429", "5xx"]
```

## Semantics

- **`env.*` / `store.*` secret references** resolve at load into
  `SecretString`s; raw keys never appear in the file, logs, or `Debug`
  output. A missing reference is a startup error naming it — never a
  runtime 500.
- **Key `weight` + `models` allowlist** feed alias-table construction in the
  router. Omitted `models` means the key serves all of the provider's models.
- **`type = "openai_compat"` + `base_url`** adds any OpenAI-compatible
  upstream by configuration. Well-known preset names (groq, mistral,
  cerebras, openrouter, ollama, vllm, …) pre-fill base URL, auth style, and
  parameter quirks; everything is overridable.
- **Validation is total at load**: unknown fields, non-positive weights,
  empty key lists, alias cycles, fallback targets that don't exist,
  deployment maps missing models — all rejected before the port binds, with
  pathed messages (`providers.groq.keys[0].weight: must be > 0`).
  `caret-router check <file>` runs the same validation standalone for CI.

## Applying changes

In `file` mode, `SIGHUP` (or `--watch`) re-reads the file; in `managed`
mode, a committed store write triggers the same machinery on every node.
Either path re-validates totally, rebuilds the routing table, and swaps it
atomically:

- Invalid new config → **keep the old, log loudly**. Reload is incapable of
  taking the gateway down.
- In-flight requests complete on their previous snapshot.
- Server-section changes (bind address, TLS) require restart; the reload log
  states exactly which changes applied and which were deferred.
- Managed-mode writes are versioned compare-and-swap — concurrent editors
  get a visible conflict, never a lost update.

## Precedence & zero-config

CLI flags → environment (`CARET_ROUTER__SERVER__PORT=…`) → file → defaults.

## Config modes

- **`managed`** (default): the file — if present — seeds the embedded
  replicated store on first boot; thereafter the store is the source of
  truth, editable from the console/CLI on any node and replicated across a
  cluster. `caret-router config export` writes it back out as TOML at any
  time.
- **`file`**: the file is the sole source of truth, hot-reloaded; the
  console is read-only. For GitOps and immutable-infrastructure shops.

Secrets may be referenced as `env.*` (injected by your platform) or
`store.*` (entered once via console/CLI, encrypted at rest, replicated).
Details in
[../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md).

With no file at all, caret-router starts on `:8080` and auto-configures any
provider whose conventional env var is present (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`, …) — the five-minute
quickstart path ([../guides/quickstart.md](../guides/quickstart.md)).
