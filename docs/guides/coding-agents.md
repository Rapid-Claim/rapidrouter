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

`OPENAI_BASE_URL` does **not** work — measured against `codex-cli 0.146.0`
on 2026-08-27, the CLI ignored it and went to `api.openai.com`. Neither does
a `model_provider` in `config.toml`: that transport opens a WebSocket
(`wss://…/v1/responses`) the gateway does not serve, and `wire_api = "chat"`
is rejected by that version as no longer supported.

The knob that *does* redirect it is **`chatgpt_base_url`** — the ChatGPT
subscription backend, which is how the CLI talks when it is signed in to a
plan rather than using an API key:

```toml
# $CODEX_HOME/config.toml
chatgpt_base_url = "http://localhost:8080"
```

Confirmed working: with that set, the CLI sends its whole backend
conversation to the gateway. The gateway accepts the model call on
`/backend-api/codex/responses`, the path that backend uses.

**One thing still blocks a complete run**, and it is an auth-flow problem
rather than a routing one. Before the first model call the CLI:

1. fetches `/plugins/featured`, `/ps/plugins/{installed,suggested,list}`,
   `/api/codex/settings/user`, `/api/codex/ps/mcp` and
   `/codex/analytics-events/events` from that base — a gateway must answer
   these or the CLI stalls; and
2. **refreshes its access token unconditionally** against a hard-coded auth
   host, so a fabricated `auth.json` is rejected and the run never reaches
   the model.

`CODEX_ACCESS_TOKEN` looks like the way round (2) but puts the CLI into
"Agent Identity" mode, which refuses a non-production ChatGPT base outright.
`CODEX_REFRESH_TOKEN_URL_OVERRIDE` exists and is the untested candidate: a
gateway that also answers the refresh could hand back a virtual key and close
the loop.

So: routing a subscription Codex CLI needs a small ChatGPT-backend shim in
the gateway — the side endpoints above plus the refresh — not just a route.

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
