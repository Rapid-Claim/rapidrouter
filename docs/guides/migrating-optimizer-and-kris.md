# Migrating the Optimizer and Kris onto the shared pool

**Status: not started.** The gateway side ships
([../components/account-pools.md](../components/account-pools.md)); neither
consumer has been touched.

Today both services hold provider credentials themselves and hand them to
agent CLIs. This is how they stop, and start drawing on the one pool
instead.

---

## 1 · Where things actually stand

| | How it gets an account today | What it spawns |
|---|---|---|
| **AGI gateway traffic** | HTTP to rapid-router `:8091` with the shared `RAPID_MASTER_KEY` | — |
| **Optimizer** | `internal/app/agipool` leases a seat per run from an on-disk **mirror of the retired gateway's** `/etc/agi-gateway` | Claude Code, Codex CLI |
| **Caret / Kris** | `~/.caret/secrets.toml` — a setup token and an API key | Claude Code, Codex CLI |

Three facts that shape everything:

1. **Nobody holds a virtual key.** All gateway traffic shares one master
   key, which names no service — so labelling accounts governs nothing
   until each service has its own `ck-…`.
2. **The optimizer's mirror is stale.** It reads the *retired* gateway's
   directory: 77 accounts where the router serves **139**.
3. **The two drive the Codex CLI in different modes.** Caret uses
   `CODEX_API_KEY` (API-key mode). The optimizer writes an `auth.json` into
   a per-run `CODEX_HOME` (ChatGPT-subscription mode). Only the first takes
   a custom base URL.

## 2 · The target

Both services keep spawning the same CLIs. What changes is *where the CLI
points* and *what credential it presents*:

```bash
# Claude Code
ANTHROPIC_BASE_URL=http://<router>:8091/anthropic
ANTHROPIC_AUTH_TOKEN=ck-…                # the service's virtual key

# Codex
OPENAI_BASE_URL=http://<router>:8091/v1
OPENAI_API_KEY=ck-…                      # same key, API-key mode
```

Both forms are already documented and supported
([coding-agents.md](coding-agents.md)). The gateway holds the ChatGPT
subscription credentials upstream; **the CLI stops needing to know
subscription seats exist at all.**

That last sentence is the whole optimizer migration. It is not "make the
Codex CLI's subscription mode talk to us" — it is "stop using subscription
mode in the CLI."

### What is genuinely unverified

The dialect matrix in CI exercises these *wire formats*, not the CLI
binaries. Nobody has run the real `claude` or `codex` binary against
rapid-router with a `ck-…` key. Do that once, first — §3.

## 3 · Step 0 — done, and here is what it found

Run 2026-08-27 against a live rapid-router (mock upstream, two services, one
labelled account each, real CLI binaries).

| CLI | Result |
|---|---|
| `claude 2.1.238` | **Works.** Requests arrived, the virtual key was attributed, and only the account labelled for that key's service served them. |
| `codex-cli 0.146.0` | **Does not work.** `OPENAI_BASE_URL` is ignored — the CLI went to `api.openai.com`. A `model_provider` in `config.toml` produced *no connection to the gateway at all*: its Responses transport opens a WebSocket (`wss://…/v1/responses`) the gateway does not serve, and `wire_api = "chat"` is rejected outright by that version. |

Also verified in the same run: label enforcement over real HTTP. One service
sent seven requests and another three; each drained only its own account's
allowance, and the unassigned account served nobody.

**Consequence for this migration.** Kris's Claude runtime can move now. The
optimizer is Codex-first, so its main path is blocked until one of these
changes:

1. **The gateway serves the WebSocket Responses transport.** This is the real
   fix and it is on our side, not OpenAI's.
2. A newer `codex-cli` honours a plain-HTTP `model_provider`.
3. The optimizer's Codex work moves to a client that speaks plain HTTP.

Until then the optimizer keeps leasing Codex seats from `agipool` — which
works — while its Claude runs can route. The code ships that way: Claude
routing is on with the two variables, and Codex routing sits behind a third,
`RAPID_OPTIMIZER_LLM_ROUTER_CODEX`, off by default and documented as unproven.

## 4 · Router-side preparation

Do this before touching either consumer. It changes nothing for anyone.

```toml
tenants = ["agi", "kris", "optimizer"]
```

Then issue three keys, one per service:

```bash
rapid-router key create --name agi-gateway     --tenant agi
rapid-router key create --name kris            --tenant kris
rapid-router key create --name optimizer-runner --tenant optimizer
```

**Do not label a single account yet.** With no labels, every pool stays
shared and every existing caller — including everything still on the master
key — keeps working exactly as it does today.

## 5 · Kris

The smallest of the two, and the right one to go first: its Codex path is
already in API-key mode.

### 5.1 The hook that already exists

`internal/platform/config/secrets.go` declares:

```go
type ProviderSecret struct {
    APIKey     string `toml:"api_key"`
    SetupToken string `toml:"setup_token"`
    BaseURL    string `toml:"base_url"`   // declared, never read
}
```

`BaseURL` is referenced in exactly two places — that declaration and a
redaction copy around line 136. **Nothing consumes it.** Wiring it through
is the whole change.

### 5.2 The spawn paths

Six places set the credential env, and they need the base URL beside it:

| File | Today |
|---|---|
| `internal/adapters/runtime/claudecode/runner.go` (~227) | `CLAUDE_CODE_OAUTH_TOKEN=` |
| `internal/adapters/runtime/codex/runner.go` (~141) | `CODEX_API_KEY=` |
| `internal/adapters/runtime/providers/claudecode.go` (~66) | `CLAUDE_CODE_OAUTH_TOKEN=` |
| `internal/adapters/runtime/providers/claudecode_purellm.go` (~103) | `CLAUDE_CODE_OAUTH_TOKEN=` |
| `internal/adapters/runtime/providers/codex.go` (~68) | `CODEX_API_KEY=` |
| `internal/adapters/runtime/providers/codex_purellm.go` (~91) | `CODEX_API_KEY=` |
| `internal/adapters/runtime/providers/registry.go` (~124) | the runtime→env-var map |

The Codex runner already carries an `ExtraEnv map[string]string` that is
appended to the child's environment, so its base URL can go through that
without restructuring anything. Claude Code's runner needs one more
`env = append(...)`.

Add the base URL **only when configured**, so an unset `base_url` leaves
today's behaviour untouched — that is your rollback.

### 5.3 Configuration

```toml
# ~/.caret/secrets.toml
[providers.claudecode]
api_key  = "ck-…"                                  # Kris's virtual key
base_url = "http://<router>:8091/anthropic"

[providers.codex]
api_key  = "ck-…"
base_url = "http://<router>:8091/v1"
```

### 5.4 Verify

Ask Kris something in Slack. Then check `GET /admin/api/requests` on the
router: the request should be there, attributed to the `kris` key. Before
this change there would be no record at all — that absence is the proof
that Kris was bypassing the gateway.

Host: `i-04e0d535a9c98b95e` (us-west-2), service `caret.service` under
`ubuntu`, config `~/.caret/configs.toml`, secrets `~/.caret/secrets.toml`.

## 6 · The optimizer

Larger, because it stops owning credentials entirely.

### 6.1 Repoint the two spawn paths

Both sit behind `agipool.SeatLease`, which today does four things per run:
lease a seat, hand the credential to the CLI, mark it healthy, release.
Only the middle step survives, and it changes shape:

| Runtime | Today | After |
|---|---|---|
| Claude Code | lease → `CLAUDE_CODE_OAUTH_TOKEN=<seat token>` | `ANTHROPIC_BASE_URL` + `ANTHROPIC_AUTH_TOKEN=ck-…` |
| Codex | lease → `writeSelectedCodexAuthJSON` into the run's `CODEX_HOME` | `OPENAI_BASE_URL` + `OPENAI_API_KEY=ck-…`; **no `auth.json` at all** |

The per-run `CODEX_HOME` machinery stays — the CLI still wants a home
directory, and `internal/app/issues/agent.go` relies on one `CODEX_HOME` per
thread. What disappears is the credential written into it.

### 6.2 Then delete `internal/app/agipool`

Thirteen files, all of them the router's job now:

```
adapters.go  health.go  jwt.go  lease.go  local.go  locallimits.go
messages.go  mirror.go  pool.go  sanitize.go  seatlease.go  view.go
```

Lease ordering, cooldowns, health probes, JWT expiry decoding, the on-disk
mirror, and the refresh-token neutralising in `sanitize.go` — the gateway
does every one of these for the traffic it already serves.

### 6.3 And the wiring around it

| Where | What to do |
|---|---|
| `internal/app/bootstrap/agipool.go` | the boot-time pool check — delete |
| `RAPID_OPTIMIZER_AGENT_ACCOUNTS_MODE` (`local-first` / `local-only`) | retire, or reduce to "router" vs "local credentials" |
| `scripts/agi-pool-sync.sh` | the mirror sync — delete |
| `web/app/optimizer-api/api/v1/codex-accounts/*` and `claude-accounts/*` | delete, or make read-only views of the router's admin API |
| `internal/app/codexmodels/manager.go` | reads the catalog through a `CODEX_HOME`; read the router's `GET /v1/models` instead |

**Do not leave two places that both claim to manage accounts.** A
half-retired account manager pointing at a dead data source is exactly the
state the optimizer is in today.

### 6.4 Two bugs this fixes on the way past

- **The stale mirror** — 77 accounts against the router's 139. Nobody
  noticed because the number looks authoritative on its own.
- **Credential collapse** — several optimizer accounts sharing one upstream
  credential, because a refreshed `auth.json` was persisted into the wrong
  slot from a shared `CODEX_HOME`. The ops runbook has a `verify` command
  that detects it. Neither is possible once the optimizer holds no
  credentials.

Host: `i-09c809d115f8fc881` (`rapid-optimizer-a`, us-east-1), API on
`127.0.0.1:8080`, Codex homes under `/home/ubuntu/.rapid-optimizer/`.

### 6.5 The thing to measure before you trust it

The optimizer leases **one seat for a whole run**. Through the gateway,
each request picks independently, and the gateway deliberately spreads load
to whichever account is furthest behind.

Prompt caching is **per account**. A 50-turn agent run with a large context
that lands on a different account every turn pays full price every turn.

So: run one real optimizer issue through the gateway and compare the token
bill against the same issue run today. If it is materially worse, the
gateway needs session stickiness — a header like `x-rapid-session: <run-id>`
pinning a run to one account — before the optimizer migrates for real. That
work does not exist yet.

## 7 · Order

| # | Step | Reversible by |
|---|---|---|
| 0 | Prove both CLIs work against a dev router (§3) | — |
| 1 | Measure the caching cost of per-request account switching (§6.5) | — |
| 2 | Declare `tenants`, issue three keys, **label nothing** (§4) | deleting the keys |
| 3 | Kris: wire `base_url`, point at the router (§5) | unset `base_url` |
| 4 | Optimizer: repoint the CLIs (§6.1) | the mode switch |
| 5 | Optimizer: delete `agipool` and its wiring (§6.2–6.3) | git |
| 6 | **Label the accounts** — 2 for Kris, 10 for the optimizer, the rest AGI | `{"tenant": null}` |

> **The one footgun.** Label an account before all three services hold their
> own key, and everything still on the master key is refused — a key with no
> service owns nothing in a labelled pool. **Keys first, labels last.**

Nothing in this sequence moves a credential. Every step is undone by
changing a setting back, which is why the labels go on at the end: until
they do, the gateway behaves exactly as it does today.

## 8 · Verifying, at each step

| Question | Where to look |
|---|---|
| Is the service reaching the gateway at all? | `GET /admin/api/requests` — its key should appear |
| Which account served? | the same view names the credential |
| Is it staying inside its own accounts? | every request for that key should name an account labelled for its service |
| Is it being refused, and why? | `403` = owns no account here; `429` = owns some, all spent |
| How many does it own? | the Accounts button on its key in the console |

## 9 · Open questions

1. ~~Codex CLI subscription mode~~ — **answered, and worse than expected**:
   the CLI cannot be pointed at the gateway at all (§3). The gateway would
   need to serve the WebSocket Responses transport.
2. **Prompt caching across a run** — unmeasured. §6.5. Could block §6.
3. **`/passthrough/…` and stateful endpoints** — the relay now spreads
   across a service's accounts. Anything relaying files, batches or
   fine-tunes needs one account per resource. No consumer has been audited.
4. **Who re-authenticates a dead account** once the optimizer stops doing
   it? The router has a device-login flow; nobody owns the watching.

## See also

- [../components/account-pools.md](../components/account-pools.md) — the design
- [coding-agents.md](coding-agents.md) — the base-URL forms, already documented
- [../components/virtual-keys.md](../components/virtual-keys.md) — issuing and scoping keys
