# Review brief — one shared pool of provider accounts

**Read this first.** It is the entry point for anyone reviewing this work
cold. Three repositories are involved and some of what is written elsewhere
describes designs that were built and then deleted; §7 says which, so you do
not spend time reviewing something already abandoned.

---

## 1 · The problem

We own ~139 Codex/Claude subscription accounts. Three services consume them:

| Service | What it is |
|---|---|
| **AGI gateway** | production application traffic |
| **Kris** (`caret-agent`) | a Slack bot; a human is waiting on it |
| **Optimizer** (`rapid-optimizer`) | batch coding-agent runs; latency-tolerant, hungry |

Today only the first reaches `rapid-router`. The other two hold provider
credentials on disk and hand them to agent CLIs they spawn. Consequences:

- **There is no way to say which accounts belong to which service.** A batch
  job can exhaust accounts production needs.
- **The optimizer's credential source is stale.** It reads a file mirror of a
  gateway that was replaced on 2026-08-18 and stopped: 77 accounts where the
  router serves 139.
- **All gateway traffic shares one master key**, which names no service, so
  nothing can be attributed or allocated.

## 2 · The goal, in one sentence

One place where every account lives, one label saying which service may spend
each one, and moving an account between services is editing one word.

## 3 · The approach

**The rule.** An account carries the name of the service it belongs to. A key
carries the name of the service it is. A request may spend the accounts whose
label matches — and no others. No sharing, no borrowing, no priorities, no
arithmetic.

The label lives **on the account** rather than in a per-service list, so an
account cannot be claimed twice and cannot be forgotten by everyone. A pool
where no account is labelled is shared exactly as before, which is how every
provider nobody has divided up stays untouched.

**Two ways a client reaches its accounts**, chosen per client:

| | Proxy | Lend |
|---|---|---|
| Mechanism | client sends requests to the gateway | gateway hands the client a credential |
| For | clients speaking a dialect we serve | a vendor CLI that cannot be pointed at us |
| Ownership enforced | yes | yes |
| Spend visible to the gateway | **yes** | no — only the lease is |

Lending exists because live testing proved a subscription-mode Codex CLI
cannot be proxied without emulating its backend's auth flow (§6).

## 4 · Where the code is

Nothing is merged, pushed, or deployed.

| Repo | Branch / worktree | Commits | State |
|---|---|---|---|
| `rapidrouter-service-accounts` | `service-accounts` | 8 | 440 tests pass, fmt + clippy clean |
| `caret-agent` | `route-through-rapid-router` | 1 | full suite passes |
| `rapid-optimizer-router` | `route-through-rapid-router` (worktree) | 2 | packages pass; 3 pre-existing failures unrelated to this |

### rapid-router

- `crates/router-core/src/router.rs` — `owned_by()` is the whole rule, five
  lines, applied where a credential is selected. `holding()`, `lease_for()`.
- `crates/router-core/src/vkey.rs` — `tenant`, `lease_accounts` on a key.
- `crates/router-core/src/config/{raw,mod,validate}.rs` — `tenants = [...]`,
  `tenant` on an account, and the validation that rejects a typo.
- `crates/router-server/src/lib.rs` — `POST /v1/accounts/lease`, and
  `/backend-api/codex/responses` accepted inbound.
- `crates/router-server/src/admin.rs` — move an account, add one already
  owned, and the per-account owner in the providers view.
- `console/src/app.tsx` — Service column on credentials, Service picker and
  an Accounts drawer on virtual keys.

### caret-agent

`internal/app/bootstrap/router_endpoint.go` — `base_url` from `secrets.toml`
is exported once on the process. All six spawn paths seed the child from
`os.Environ()`, so one place covers them and any runner added later.

### rapid-optimizer

`internal/platform/llmrouter/` — routed environments. Wired at
`claudeauth.EnvWithOAuthToken` (every Claude spawn settles its credential
there) and `codexapp` (`prepareCodexHome`, `runner.go`). Claude routing is on
with two env vars; **Codex routing is behind a third and off**, because it
does not work (§6).

## 5 · What was verified, and how

No Rust toolchain on the dev machine, so everything runs in a container:

```bash
docker run --rm -m 10g -v "$PWD":/work -w /work \
  -v rr-cargo-registry:/usr/local/cargo/registry \
  -v rr-target:/target -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=1 \
  rust:1 sh -c "rustup component add rustfmt clippy && \
    cargo fmt --all --check && cargo clippy --workspace --all-targets && \
    cargo test --workspace"
```

Beyond the suites, a **live run on 2026-08-27**: a real `rapid-router` binary,
two services, one labelled account each, and the real CLI binaries.

- **Label enforcement over real HTTP.** One service sent 7 requests, another
  3. Each drained only its own account's rate allowance; the unassigned
  account served nobody.
- **Claude Code 2.1.238 works.** Requests arrived, the virtual key was
  attributed, and only the account labelled for that key's service served.
- Two bugs were found this way that the tests missed — §6.

## 6 · What the live run found

1. **`codex-cli 0.146.0` resisted every proxying attempt we made — but that
   conclusion is now in doubt, and it is the single most important open item.**
   What we measured: `OPENAI_BASE_URL` is ignored (it went to `api.openai.com`
   and spent a real account); a `model_provider` block in `config.toml` wanted
   a WebSocket we do not serve; `wire_api = "chat"` is rejected as no longer
   supported. `chatgpt_base_url` **does** redirect it — verified — but the CLI
   then refreshes its token against a hard-coded auth host and refuses a
   fabricated `auth.json`, and `CODEX_ACCESS_TOKEN` switches it into "Agent
   Identity" mode which rejects a non-production base. Lending was built as a
   consequence of this.

   **Why it is in doubt.** Every attempt above tried to keep the CLI in
   *subscription* mode. Both LiteLLM and CLIProxyAPI drive this same CLI
   successfully in **API-key mode** — a `model_provider` pointing at a
   `/v1` base with an `env_key`, where the "API key" is a gateway virtual key
   and the gateway holds the subscription seats. That is what the optimizer's
   ADR already says it wants. Our `model_provider` attempt most likely failed
   on a missing `wire_api = "responses"` rather than on a CLI limitation.

   **RESOLVED 2026-08-27 — the CLI can be proxied. Our earlier conclusion was
   wrong; the missing piece was `wire_api = "responses"`.** Ran codex-cli
   0.146.0 against a local listener with a clean `CODEX_HOME` containing only:

   ```toml
   model = "gpt-5"
   model_provider = "rr"
   [model_providers.rr]
   base_url = "http://127.0.0.1:8099/v1"
   env_key = "RR_KEY"
   wire_api = "responses"
   ```

   No `auth.json`, and `OPENAI_API_KEY` / `CODEX_ACCESS_TOKEN` / `CHATGPT_BASE_URL`
   scrubbed from the environment, so a fallback to the vendor would have failed
   loudly rather than silently spending a seat. Result: exactly two requests,
   both to the listener — `GET /v1/models` and `POST /v1/responses`, the latter
   carrying `Authorization: Bearer <the value of RR_KEY>`, `accept:
   text/event-stream`, and a normal Responses body. Zero contact with
   `openai.com` or `chatgpt.com`. The CLI consumed our SSE stream and exited 0.

   **Consequences.**
   - Codex can run fully proxied: per-request account selection and complete
     spend visibility. `POST /v1/accounts/lease` has no remaining justification
     and should be **deleted rather than reviewed**.
   - **`OPENAI_BASE_URL` genuinely does not work** — it must be a
     `model_provider` block written into the run's `CODEX_HOME/config.toml`.
     Both Go changes currently set env vars for Codex and are wrong on this
     point: `caret-agent/internal/app/bootstrap/router_endpoint.go:35` and the
     optimizer's `CodexEnv()`.
   - The request body carries **`prompt_cache_key`**, which is the natural
     handle for pinning a conversation to one seat — see §8.6.

   **Still unproven:** that rapid-router's own `/v1/responses` handler
   translates such a request onto a Codex *subscription* seat upstream. The
   listener was a mock. That is the next test, and it needs a real seat.
2. **The services roster was attached to the wrong handler** — an edit matched
   the first of several identical `Json(json!({ "data": data }))` tails, so it
   landed on the users endpoint. The console dropdown would have been silently
   empty. No test asserted on it.

## 7 · Designs already built and deleted — do not re-propose

| Rejected | Why |
|---|---|
| A per-key hash slice (`max_accounts`) | Invisible and unmovable; measured to strand ~40% of a 70-account pool while oversubscribing the rest. |
| Floors + priority + cutoffs | Protected *access* at the end of the pool, never limited *consumption*, which was the actual worry. Sizing was a trap: "AGI gets the rest" made the lowest-priority service permanently paused. |
| Borrowing between services, ownership leases, a reconciler, hysteresis | All artifacts of partitioning the pool and then needing to un-partition it. Deleting the partition deleted them. |
| A utilization gate on lending capacity | Measured wrong: the router normalizes load, so a pool crosses any threshold together — open when nobody needs it, shut when they do. |
| An `accounts = […]` pin on keys | A second mechanism answering the same question as the label. |

**Note the collision:** "lease" above means *moving ownership between
services*, which is gone. `POST /v1/accounts/lease` is a different thing —
lending a credential to a client that cannot be proxied.

### Adopting an existing gateway instead

Evaluated 2026-08-27, not chosen:

- **LiteLLM.** Its unit is "a model plus an `api_key`". Its Claude Max support
  is *pass-through*: the client holds the OAuth token and the proxy forwards
  the header — one seat per user, travelling with the caller — which is the
  inverse of holding 139 seats and choosing one, and it is
  [currently broken](https://github.com/BerriAI/litellm/issues/19618). Its
  Codex tutorial is likewise an API key. Tag-based routing is the right shape
  but tags are caller-supplied in the body or an `x-litellm-tags` header;
  key-level enforcement is
  [an open request](https://github.com/BerriAI/litellm/issues/22966) and the
  team-scoped form is enterprise — so the one property this design rests on
  would depend on someone else's roadmap. Adopting it would also mean
  rebuilding OAuth refresh, per-seat weekly quota tracking and benching inside
  a deployment abstraction that has no place for them.
- **CLIProxyAPI.** The closest thing that exists: real OAuth seat pools for
  Codex/Claude/Gemini with rotation, failover and quota detection — the same
  category as rapid-router. It has pooling but **no per-consumer ownership**,
  which is the feature being added here. Worth reading for prior art on
  driving codex-cli (§6.1); not worth swapping a production gateway for.

Its Codex tutorial is still the most useful thing LiteLLM contributed here —
see §6.1.

## 8 · Where I want scrutiny

1. **Should the lend endpoint exist at all?** It was built because we
   believed codex-cli could not be proxied. §6.1 now says that belief may rest
   on a misconfiguration. Before reviewing the endpoint's guards, judge whether
   the endpoint is needed — deleting it is better than hardening it.
2. **Is partitioning the right default, or is budgeting?** The worry that
   started this was consumption: one service draining the pool. A partition
   answers it by walling off capacity, which strands capacity — two seats idle
   overnight while another service is throttled — and the real constraint on a
   subscription seat is a *weekly token quota*, so a badly sized partition
   wastes money already spent. `vk_gate` already enforces rpm/tpm **and**
   budgets per key. A weekly token budget per service matches the actual
   constraint far better and strands nothing; what it cannot give is
   availability at the moment of need, if one service has benched every seat
   on 429s while still under budget. The proposed default is therefore a
   hybrid, which the built code already permits because unlabelled accounts
   stay shared: **label nothing by default, set per-service budgets, and
   reserve a small number of seats only where a human is waiting.** Is that
   right, and if so is anything in the code pulling against it?
3. **Is lending a credential over HTTP acceptable at all?** It is gated on a
   per-key `lease_accounts` flag and the refresh token is blanked, but it is
   still an endpoint that hands out account material to any holder of a valid
   key with that flag. Is that guard sufficient, or does it want a stronger
   one (mTLS, an allowlist, short-lived tokens)?
4. **The "unassigned account serves nobody" rule.** New accounts arrive owned
   by no one and are inert until labelled. Safe, but it means buying capacity
   has a second step someone must remember. Right trade?
5. **A key with no service owns nothing in a labelled pool.** This is what
   makes "keys first, labels last" a hard ordering requirement during rollout.
   Is a loud failure right, or should it degrade to the unassigned pool?
6. **Prompt caching, unmeasured.** Proxied multi-turn agent runs spread across
   accounts and caching is per account, so a 50-turn run may pay full price
   every turn. Nobody has measured it. It is an argument for lending the
   optimizer's Codex work regardless — is it also an argument against proxying
   its Claude work?
7. **The optimizer's remaining change** — swapping `agipool`'s source from the
   file mirror to the gateway — is designed but not written. Does the
   plan in `docs/guides/migrating-optimizer-and-kris.md` hold up?
8. **Three pre-existing test failures** in the optimizer, verified identical on
   untouched HEAD. Confirm they are unrelated.

## 9 · The rest of the documentation

| Document | What it is |
|---|---|
| `docs/components/account-pools.md` | the design as built — current |
| `docs/guides/migrating-optimizer-and-kris.md` | the migration sequence — current |
| `docs/guides/coding-agents.md` | how each CLI connects, with what was measured and when |
| `COMPLETED.md` | what changed, every command run, and what is left |
| `IMPLEMENTATION_PLAN.md` | **partly superseded** — written before the lease endpoint and the console work; its test-gate section is still accurate |
| `rapid-optimizer-router/docs/adrs/2026-08-27-route-agent-runs-through-rapid-router.md` | the optimizer's side |

## 10 · Not done

- **End-to-end Codex through the real router** — §6.1 proved the CLI reaches
  an arbitrary `/v1/responses`; it has not been proved that rapid-router serves
  such a request from a Codex subscription seat. Needs a real seat.
- **Deleting `POST /v1/accounts/lease`** and the Go code that would consume it,
  now that §6.1 removes its justification.
- Nothing merged, pushed or deployed; three branches.
- The optimizer's `agipool` source swap (§8.7).
- Per-service usage metrics; no browser test drives the new console controls.
- The rapidrouter main checkout holds other people's uncommitted work; this
  branch was lifted out of it and carries some of theirs in the diff.
  **This was understated in an earlier draft as "~31 lines in three config
  files".** It is closer to ~110 lines of a `trace_keys` / `trace_value_chars`
  feature across `config/{mod,raw,validate}.rs`, ~80 lines of tests for it in
  `tests/config_validation.rs`, and the majority of the console diff
  (`meta`/`MetaFilter`/facets/detail-drawer in `api.ts` and `app.tsx`, and most
  of `styles.css`). All of it is inert on this branch — nothing consumes
  `usage.trace_keys` and `UsageRecord` has no `meta`/`seat`/`ttft_ms` field —
  but a reviewer told "31 lines" will misattribute several hundred.
