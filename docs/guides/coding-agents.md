# Coding Agents Through the Gateway

Agents are the heaviest, most dialect-sensitive LLM clients you run —
streaming tool use, parallel calls, prompt caching, very long sessions.
Routing them through rapid-router gives you provider failover, key
management, spend metering, and model swapping without touching the agent.
Each client below is a scripted scenario in our CI
([../api/02-dialects.md](../api/02-dialects.md)).

## Claude Code

```bash
export ANTHROPIC_BASE_URL=http://localhost:8080/anthropic
export ANTHROPIC_AUTH_TOKEN=<gateway key, if auth is enabled>
claude
```

- Requests arrive in Anthropic's dialect; prompt-caching headers pass
  through, so cache hits (and their cost/latency wins) are preserved.
- **Model swapping**: point an alias at anything —

  ```toml
  [aliases]
  "claude-sonnet-4-5" = "openai/gpt-4o"   # Claude Code now drives gpt-4o
  ```

  Cross-dialect translation handles streaming tool use and thinking blocks;
  capability gaps fail precisely, not silently.

## Codex

Codex speaks the Responses API:

```bash
export OPENAI_BASE_URL=http://localhost:8080/v1
codex
```

`/v1/responses` relays the full surface to OpenAI/Azure targets and
translates the stateless core elsewhere
([../api/01-endpoints.md](../api/01-endpoints.md)).

## Anything OpenAI-compatible (Cursor, Continue, aider, …)

Set the tool's OpenAI base URL to `http://localhost:8080/v1` and pick models
by `provider/model` string or alias. Tools that hardcode model lists work
best with aliases that shadow the expected names.

## Why bother

- **One key in the agent, all providers behind it** — rotate/replace
  provider keys without reconfiguring machines.
- **Failover mid-outage**: fallback chains keep agents working through a
  provider incident.
- **Spend visibility per agent**: give each machine its own gateway key and
  read `rapid_cost_usd_total` per key.
- **Latency receipts**: `x-rapid-overhead-us` proves the gateway isn't your
  bottleneck.
