# Coding Agents Through the Gateway

Agents are the heaviest, most dialect-sensitive LLM clients you run —
streaming tool use, parallel calls, prompt caching, very long sessions.
Routing them through rapid-router gives you provider failover, key
management, spend metering, and model swapping without touching the agent.
The dialects below are scripted scenarios in our CI
([../api/02-dialects.md](../api/02-dialects.md)) — that covers the wire
formats, not the CLI binaries, which is why each entry says what was actually
run against a live gateway and when.

## Claude Code

```bash
export ANTHROPIC_BASE_URL=http://localhost:8080/anthropic
export ANTHROPIC_AUTH_TOKEN=<gateway key, if auth is enabled>
claude
```

**Verified** against `claude 2.1.238` on 2026-08-27: requests arrived, the
virtual key was attributed, and the account labelled for that key's service
served them.

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

**Not currently working — measured, not assumed.** Against `codex-cli
0.146.0` on 2026-08-27:

- `OPENAI_BASE_URL` is **ignored**. The CLI went to `api.openai.com` and
  never opened a connection to the gateway.
- A `model_provider` in `config.toml` pointing at the gateway produced no
  connection either: the Responses transport opens a **WebSocket**
  (`wss://…/v1/responses`) that the gateway does not serve.
- `wire_api = "chat"` is rejected by that version — *"no longer supported"*.

So a Codex CLI cannot presently be pointed here. Serving the WebSocket
Responses transport is what would close the gap.

`/v1/responses` itself is unaffected and works for any client that speaks
plain HTTP Responses — it relays the full surface to OpenAI/Azure targets and
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
