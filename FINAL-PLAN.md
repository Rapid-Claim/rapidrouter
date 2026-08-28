# The account pool — final plan

*2026-08-27. Supersedes `IMPLEMENTATION_PLAN.md`, which was written before the
lease endpoint, the console work, and the two live test rounds.*

## The idea

Every provider account carries a label: the name of the service that owns it.
Every virtual key carries the name of the service it is. A request may spend
the accounts whose label matches, and no others. An account nobody has
labelled is shared exactly as it is today — which is what makes the whole
change inert until somebody uses it.

That is the entire mechanism. There is no borrowing, no priority order, no
floors, no arithmetic, and nothing to reconcile. Moving an account between
services is editing one word.

## How each service reaches its accounts

All three proxy. Nothing is lent. Verified against the real CLIs:

| Service | Runtime | How it is pointed at the router |
|---|---|---|
| AGI gateway | its own HTTP client | already proxying; needs a `ck-` key instead of the shared master key |
| Kris | Claude Code | `ANTHROPIC_BASE_URL=<router>/anthropic`, `ANTHROPIC_AUTH_TOKEN=<ck-key>` — **verified working** |
| Kris, Optimizer | codex-cli | a `model_provider` block in the run's `CODEX_HOME/config.toml` with `wire_api = "responses"` — **verified reaching an arbitrary base**; `OPENAI_BASE_URL` does **not** work |
| Optimizer | Claude Code | same as Kris |

The gateway holds the subscription seats. No CLI holds a credential, writes an
`auth.json`, or needs to know that subscriptions exist.

**`POST /v1/accounts/lease` is deleted.** It was built on the belief that
codex-cli could not be pointed at us. That belief was wrong (REVIEW-BRIEF
§6.1), and removing the endpoint removes five of the review's fifteen
findings, including its only credential-disclosure blocker.

## The policy: budget by default, label by exception

The worry that started this was consumption — one service draining the pool.
Two controls answer it and they are not the same:

- **A budget** caps how much a service may spend. `vk_gate` already enforces
  rpm, tpm and budgets per key. A weekly token budget matches the real
  constraint on a subscription seat, which is a weekly quota, and it strands
  nothing: idle capacity stays available to whoever needs it.
- **A label** guarantees a service can get a request through *right now*, even
  when everyone else has benched every seat on 429s. It also strands whatever
  it reserves.

So: **budget everything, label almost nothing.** Reserve seats only where a
human is waiting on the answer — Kris's two. The batch services get the whole
pool with a ceiling on how much of it they may consume.

## Work remaining

### rapid-router

1. Delete the lease endpoint, `lease_for`, `without_refresh_token`,
   `lease_accounts`, and their eight tests. *(F1, F6, F9, F13, F14)*
2. Correct the text that still describes the deleted floors/priority design —
   `console/src/app.tsx:1868` first, since it sits under the control itself,
   then `virtual-keys.md`, `agent-subscriptions.md`, `vkey.rs`,
   `validate.rs`, `api.ts`. *(F7)*
3. Validate `--tenant` on the CLI path, which writes straight to the store
   today, and fix the `[tenants.<name>]` syntax in its help. *(F10)*
4. Warn at config load when any provider is `managed` while any key has no
   tenant, so the first symptom is a startup line rather than production 403s.
5. Make the console confirm before the first label on a shared pool, saying how
   many callers it cuts off; it already receives `managed` and ignores it.
   Warn on an unlabelled account inside a labelled pool — today it renders
   green and serves nobody. *(F11)*

### caret-agent

6. Call `applyProviderEndpoints` from the real boot path, with a test that
   asserts boot reaches it. Today it is only reachable from a test. *(F2)*
7. Remove the vendor credential from the three live spawn paths and delete the
   `setup_token` fallback, which currently ships the Anthropic OAuth token to
   the gateway as its bearer. *(F3)*
8. Point Codex with a `config.toml`, not `OPENAI_BASE_URL`. Note the process
   environment is inherited by the agent's bash tool and PTY.

### rapid-optimizer

9. Stop requiring a leased seat on the routed path — every caller hard-refuses
   on an empty token before reaching the chokepoint, so routed runs are still
   gated by the stale 77-account mirror. *(F5)*
10. Before enabling routed Codex: clear `auth.json` from the persistent
    `CODEX_HOME`, and gate `captureRefreshedCredential` on `routed`, or a stale
    credential gets written into another account's record. *(F4)*
11. Do not bench pool accounts on gateway errors when routed. *(F8)*
12. `agipool` then has no routed caller and can be deleted in a later change —
    not swapped to another source.

### Verification before rollout

13. Codex end-to-end through the real router onto a real subscription seat.
    Only the CLI half is proven.
14. Ten minutes with the real Claude binary: does `ANTHROPIC_AUTH_TOKEN` beat
    `~/.claude/.credentials.json` when both are present? If not, routed runs
    silently spend the host login.
15. Measure prompt caching on one real multi-turn run, proxied versus not,
    before routing becomes the default. If the bill is real, build session
    affinity on `prompt_cache_key` — the CLI already sends it — rather than
    reintroducing lending.
16. Record the names of the three pre-existing optimizer test failures.

## Rollout order

The order is forced, not preferred: **master-key traffic can never carry a
service name.** There is no field for it, and `gateway_auth` sets the tenant to
`None` on a static key match. So the moment any account in a pool is labelled,
every master-key caller loses access to it, with no partial state available.

1. Land the fixes above. Nothing is labelled; behaviour is unchanged.
2. Issue three `ck-` keys, one per service, each naming its service.
3. Move all three services onto their keys. Still nothing labelled, so this
   changes nothing except that traffic is now attributable. **This is the
   reversible step and the one worth sitting on** — watch
   `/admin/api/requests` until every caller is accounted for.
4. Set a weekly budget per key.
5. Only now, label Kris's two seats. Everything else stays shared.

Step 3 is the whole migration. Steps 4 and 5 are policy and take a minute
each.

## What this deliberately does not do

Per-service usage metrics beyond what `/admin/api/requests` already shows;
automatic rebalancing; any account movement that isn't a human editing a
label. See `REVIEW-BRIEF.md` §7 for the designs that were built, measured and
deleted, so they are not proposed again.

---

# Progress — 2026-08-28

## Done

**Router.** The lease endpoint is gone: `lease_for`, `without_refresh_token`,
`lease_accounts`, the handler, the route, and their eight tests — 457 lines
deleted. That closes **F1** (live Claude refresh tokens handed out), **F6**
(lending ignored ownership on an unlabelled pool), **F9**, **F13** and **F14**,
none of which needed a fix once the feature had no reason to exist.

**Optimizer.** Four fixes:

- `LeaseClaude` / `LeaseCodex` return a routed lease when a gateway is
  configured, so a routed run takes no seat (**F5**). It carries no store, so
  release, health and refresh are all no-ops — which is also **F8**: a gateway
  429 can no longer bench a pool account that never served the request. Three
  of the four runners go through this chokepoint; `issues/agent.go` leases
  inline and was fixed alongside, as was the rotation in `workers`.
- Codex is pointed by `CodexConfigTOML()` written into the run's `CODEX_HOME`,
  not by `OPENAI_BASE_URL`, which codex-cli ignores. Written on **every**
  routed run, including one with no MCP servers.
- `prepareCodexHome` clears `auth.json` and `credentials.json` from a reused
  session home when routed, and `captureRefreshedCredential` returns early
  (**F4**) — the two halves that were writing one account's credential into
  another account's record.
- `CODEX_ACCESS_TOKEN` was added to the vendor-credential strip list.

## Verified end to end

A real `rapid-router` binary in a container, port-published, serving a
`codex_subscription` and a `claude_subscription` provider against the
mock upstream, two seats each, one labelled `optimizer` and one `kris`.

**Ownership, over real HTTP.** With the optimizer's seat capped at `rpm = 2`:

| caller | result |
|---|---|
| optimizer key ×5 | `200 200 503 503 503` — confined to its own seat |
| kris key ×5 | `200 200 200 200 200` — untouched by the optimizer exhausting its own |
| key with no service ×2 | `403 403` — owns nothing in a divided pool |

**The optimizer's own runners**, both now passing:

- `TestE2ECodexRunsThroughTheRouter` — the real `codexapp.Runner` spawning real
  `codex app-server`, which reached the gateway and got assistant text back.
  Note this is the app-server transport, not `codex exec`; they are different
  and only this one is what the optimizer spawns.
- `TestE2EClaudeRunsThroughTheRouter` — starting from `LeaseClaude(nil)` with
  no pool configured at all, through the routed lease, the runner's credential
  guard, claudeauth's env swap, and the real `claude` binary. It asserts
  against the **gateway's** request log rather than the CLI's output, because
  a static mock never gives the CLI a conversational stop and that says nothing
  about routing either way.

Both are skipped unless `RAPID_E2E_ROUTER_URL` / `RAPID_E2E_ROUTER_KEY` (and
`RAPID_E2E_ADMIN_KEY` for the Claude one) are set.

## Two findings the e2e produced

1. **`ANTHROPIC_AUTH_TOKEN` beats `~/.claude/.credentials.json`.** Measured
   with an isolated `HOME` holding a decoy host login: the decoy was never
   used. This closes the review's one unverified item — no `CLAUDE_CONFIG_DIR`
   isolation is needed.
2. **The mock provider's Codex stream was not faithful**, and no router test
   caught it because no router test drives the real CLI. `response.completed`
   omitted `id`, which codex-cli 0.146.0 refuses outright; and the text branch
   emitted deltas with no `output_item.added` / `.done` around them, so a
   client had nothing to attach them to and saw an empty turn. Both fixed, with
   the measurement recorded at the fix.

## Test state

- Rust workspace: **432 passed, 0 failed**; `cargo fmt --check` and
  `cargo clippy --all-targets -- -D warnings` both clean.
- Optimizer: **73 packages pass.** Pre-existing failures, all reproduced on
  untouched `HEAD` and therefore not from this work — two deterministic in
  `internal/app/harness` (`TestListTestSuitesReturnsExpectedCodeCounts`,
  `TestAgentCanRemoveTestCasesAndDeleteSuites`) and a set of load-dependent
  3.00s timeouts in `internal/adapters/runtime/workers` that pass in isolation.
  That closes the review's request to name them.

## Still to do

- **caret-agent: F2 and F3.** Untouched today. The change still never runs, and
  still sends the vendor OAuth token to the gateway as its bearer. Codex there
  also needs the `config.toml` treatment rather than `OPENAI_BASE_URL`.
- **Router: F7, F10, F11** — the console label and three docs describing the
  deleted floors design; CLI `--tenant` validation; the console warning before
  the first label divides a pool.
- One real Codex subscription seat, to confirm the gateway's upstream leg. The
  seat in every test above is a fixture against the mock.
- Prompt caching, unmeasured.

## Codex fidelity — what the gateway actually sends upstream (2026-08-28)

The mock was replaced with a recorder standing in for `chatgpt.com`, so the
exact request the gateway would put on the wire could be read. From a real
`codex app-server` run driven by the optimizer's own runner:

```
POST /backend-api/codex/responses
  authorization:       Bearer <seat-opt's access token>   (decodes to acct-opt)
  chatgpt-account-id:  acct-opt
  originator:          codex_cli_rs
  user-agent:          codex_cli_rs/0.146.0
  version:             0.146.0
  openai-beta:         responses=experimental
  session_id:          01a0495a…
  body: model, instructions, input, reasoning, text, store, stream  (28 KB, 6 items)
       prompt_cache_key present and forwarded
```

The gateway impersonates the CLI to the backend, which is what a subscription
seat requires. **Six runs, six upstream requests, every one on `acct-opt`** —
kris's seat was never touched.

Under an injected upstream 429: two attempts, both on the optimizer's own seat,
never kris's, surfaced to the caller as a clean error. Rotation stayed off, so
no pool account was benched for a gateway failure.

Production-condition tests, all passing (`-run TestE2EProd`):

| | why it would bite |
|---|---|
| routing survives an MCP config | every real run has one; if the MCP writer wins, the run goes to the vendor silently |
| `model_provider` precedes the MCP tables | a key after a `[table]` header belongs to that table |
| a second turn on a reused session home | the persistent home is never cleaned |
| three concurrent runs | sessions must not collide in that home |
| unsetting the variables restores the pool | the rollback |

