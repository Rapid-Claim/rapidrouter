# Quickstart

## 1 · Install

```bash
# macOS / Linux
curl -fsSL https://rapid-router.dev/install.sh | sh     # or: brew install rapid-router
# Docker
docker pull ghcr.io/rapid/rapid-router
```

(Verify signatures if you're the careful kind — releases are cosign-signed
with a published SBOM.)

## 2 · Run — zero config

```bash
export OPENAI_API_KEY=sk-...
export ANTHROPIC_API_KEY=sk-ant-...
export GROQ_API_KEY=gsk-...

rapid-router
# rapid-router listening on 0.0.0.0:8080
# providers configured from environment: openai, anthropic, groq
```

Any provider whose conventional env var is set is live immediately.

## 3 · Call it

```bash
curl http://localhost:8080/v1/chat/completions \
  -H "Content-Type: application/json" \
  -d '{
    "model": "anthropic/claude-sonnet-4-5",
    "messages": [{"role": "user", "content": "one-line haiku about gateways"}],
    "stream": true
  }'
```

Same endpoint, any provider — change the `model` string:
`openai/gpt-4o-mini`, `groq/llama-3.3-70b-versatile`, `gemini/gemini-2.5-pro`.

## 4 · Point your SDK at it

```python
# OpenAI SDK — only the base_url changes
from openai import OpenAI
client = OpenAI(base_url="http://localhost:8080/v1", api_key="unused-or-gateway-key")
r = client.chat.completions.create(
    model="anthropic/claude-sonnet-4-5",
    messages=[{"role": "user", "content": "hello"}],
)
```

```python
# Anthropic SDK — speaks its own dialect via the /anthropic prefix
from anthropic import Anthropic
client = Anthropic(base_url="http://localhost:8080/anthropic", api_key="unused-or-gateway-key")
```

## 5 · Configure — console or file

Everything beyond zero-config — weights, fallbacks, aliases, keys, budgets
— is editable in the **embedded web console**: set an admin key and open
`http://localhost:8080/console`. Changes persist in the gateway's own
storage (and replicate across a cluster) — no database to run
([../components/console.md](../components/console.md)).

Prefer files? Same schema, `file` mode
([../components/config.md](../components/config.md)):

```toml
[aliases]
fast = "groq/llama-3.3-70b-versatile"

[fallbacks]
"openai/gpt-4o" = ["anthropic/claude-sonnet-4-5"]
```

```bash
rapid-router --config rapid-router.toml --watch
```

Now `"model": "fast"` routes wherever you point it, and gpt-4o traffic
survives an OpenAI incident without your application noticing — check the
`x-rapid-provider` response header to see who actually served.

## Next

- Route coding agents through the gateway: [coding-agents.md](coding-agents.md)
- Understand the receipt headers: [../components/observability.md](../components/observability.md)
- Going multi-node? Point them at one bucket or table: [../operations/fleet.md](../operations/fleet.md)
