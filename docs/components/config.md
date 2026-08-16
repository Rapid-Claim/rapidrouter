# Configuration

Configuration is one document — TOML canonical, JSON accepted with the
identical schema — writable two ways: as a file you edit (`file` mode), or
through the embedded console/CLI, persisted and replicated by the built-in
store (`managed` mode, the default). Same schema either way; no external
database or config service in either mode.

## Example `rapid-router.toml`

```toml
[server]
host = "0.0.0.0"
port = 8080
max_body_size_mb = 100
auth_keys = ["env.RAPID_GATEWAY_KEY"]     # omit for open localhost use
require_auth = false                      # true: refuse anonymous requests
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

[console]                       # the console and /admin/api exist only
admin_keys = ["env.RAPID_ADMIN_KEY"]      # once admin credentials do
session_ttl_secs = 43200

[usage]
retention_days = 30             # local partitions pruned by the binary
flush_interval_secs = 10
per_key_metrics = false         # bounded per-key metric labels

[pricing."openai/gpt-4o-mini"]  # overrides the built-in price table
input_per_mtok = 0.15
output_per_mtok = 0.60

[[virtual_keys]]                # file-mode form; console/CLI is the usual path
name        = "checkout-service"
id          = "9f3a2c"
secret_hash = "blake3:…"        # from `rapid-router key hash`
models      = ["openai/gpt-4o-mini", "fast"]
budget      = { usd = 250, period = "monthly" }
rate_limit  = { rpm = 600, tpm = 400_000 }
expires     = "2027-01-01T00:00:00Z"
tags        = { team = "payments" }
enabled     = true
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
  `rapid-router check <file>` runs the same validation standalone for CI.
- **Virtual keys carry hashes, never secrets.** A `[[virtual_keys]]` entry
  is validated like everything else: the id must be six hex characters, the
  hash must be `blake3:` plus 64 hex characters, and every scope entry must
  name a configured provider or alias. Managed-mode keys live in the store
  instead and win over a file entry with the same id, because rotation
  state lives there. See
  [virtual-keys.md](virtual-keys.md).
- **`[console] admin_keys` is the console's on-switch.** With none
  configured, `/console` and `/admin/api/*` do not exist — separate
  credentials from data-plane keys, by construction.
- **`[pricing]` overrides the built-in table** per `provider/model`, in USD
  per million tokens. Unknown models simply cost nothing in reports rather
  than guessing.

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

CLI flags → environment (`RAPID_ROUTER__SERVER__PORT=…`) → file → defaults.

## Config modes

- **`managed`** (default): the file — if present — seeds the embedded
  replicated store on first boot; thereafter the store is the source of
  truth, editable from the console/CLI on any node and replicated across a
  cluster. `rapid-router config export` writes it back out as TOML at any
  time.
- **`file`**: the file is the sole source of truth, hot-reloaded; the
  console is read-only. For GitOps and immutable-infrastructure shops.

Secrets may be referenced as `env.*` (injected by your platform) or
`store.*` (entered once via console/CLI, encrypted at rest, replicated).
Details in
[../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md).

With no file at all, rapid-router starts on `:8080` and auto-configures any
provider whose conventional env var is present (`OPENAI_API_KEY`,
`ANTHROPIC_API_KEY`, `GEMINI_API_KEY`, `GROQ_API_KEY`, …) — the five-minute
quickstart path ([../guides/quickstart.md](../guides/quickstart.md)).
