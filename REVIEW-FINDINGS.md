# Review findings — 2026-08-27

An independent read of `9af5cc0..HEAD` plus both Go commits and the five
documents in `REVIEW-BRIEF.md` §9. No builds or tests were run; everything
below was established by reading code and by `git log -S`.

**Verdict: not ready.** The design holds — `owned_by()` genuinely is a single
gate on the data plane, and an unlabelled pool genuinely behaves as before.
The gap is in the parts around it.

## What the enforcement audit found

Every mounted route was traced to the point a credential is chosen. All of
`run_chat`, `run_responses`, `run_relay`, `run_stream_relay`,
`handle_provider_relay` and `run_passthrough` pass through `admit_key` /
`admit_any` with the tenant applied. Retries and fallbacks inherit it —
`tenant` is computed once per request and reused, and `attempt_budget`,
`all_keys_benched` and `holding` are all tenant-scoped. **`tenant` is never
read from a header or body**; the only sources are `vk.def.tenant` and the
config, so a caller cannot assert a service. The three unfiltered selections
(`select_key`, `probe_key`, `device_login`) are admin-authenticated or
test-only, and `spawn_seat_maintenance` touching every seat is correct.

No path was found where one service can spend another's **labelled** account.

## Blockers

**F1 — `without_refresh_token` hands out live Claude refresh tokens.**
`router-core/src/credential.rs:714-741` blanks three snake_case paths. The real
Claude document, per this repo's own parser at `credential.rs:206-233`, is
`{"claudeAiOauth":{"refreshToken":…}}`. No path matches. `cleared` is computed
then discarded at line 739, so the function returns the **unchanged** document
instead of `None`. The optimizer's last-mile guard (`agipool/sanitize.go:71-89`)
shares the identical blind spot, because it is the origin of the port and was
correctly scoped to Codex only — the scope note was dropped, the field list was
not widened. All eight tests use a Codex `auth.json`.
*Effect:* a borrower's CLI rotates the token, the gateway's copy dies, operator
re-login per account.

**F2 — the caret-agent change never runs.** `applyProviderEndpoints` is called
only from `setupAIProviders` (`gateway_bootstrap.go:187`), whose only caller in
the tree is a test. The real boot path is `cmd/caret/gateway.go:85` →
`BuildGatewayRuntime`, which returns at `gateway_bootstrap.go:175` without
reaching it. All six tests call the function directly.
*Effect:* set `base_url`, restart, and Kris talks to Anthropic as before —
silently.

**F3 — caret-agent sends the vendor OAuth token to the gateway as the bearer.**
`claudecode/runner.go:227` appends the real `CLAUDE_CODE_OAUTH_TOKEN` after
`os.Environ()`, so it wins; `router_endpoint.go:19-21` claims the opposite of
what lines 54-57 admit. `providerKey` (lines 71-76) falls back to
`GetProviderSetupToken`, so the common config — existing install, one added
`base_url` line — exports the Anthropic OAuth token as `ANTHROPIC_AUTH_TOKEN`.
Line 35 also maps codex to `OPENAI_API_KEY` while every codex spawn reads
`CODEX_API_KEY` (and per REVIEW-BRIEF §6.1 neither works — Codex needs a
`config.toml`). Secondary: moving credentials to the *process* environment
exposes them to the agent's bash tool (`tools/exec.go:111,118`) and PTY.

**F4 — routed Codex reuses a stale credential and corrupts another account's
record.** `codexapp/mcpconfig.go:67-74` skips writing `auth.json` when routed
but never removes an existing one, and `codexHomeDir()` returns the persistent
never-cleaned home whenever `SessionKey != ""` — which every production caller
sets. `captureRefreshedCredential` (lines 143-156) is gated on
`AllowCredentialRefresh` but **not** on `routed`, so account A's stale file is
written into account B's record. This is the credential-collapse bug the
migration guide says routing prevents, now caused by it. Contained only because
the Codex opt-in defaults off.

**F5 — routed Claude runs still require the stale 77-account mirror.**
`claudeauth/env.go:26-29` discards the token, but every caller leases a seat
first and hard-refuses on empty: `claudecode/runner.go:221-225`,
`issues/agent.go:1278`, `workers/manager.go:474`, `triagerunner/manager.go:212`.
*Effect:* the 62 accounts the router has and the mirror lacks stay unreachable,
and a fully-routed deployment still fails every run when the mirror is dry —
the exact failure routing exists to remove. The ADR claims otherwise.

**F7 — four shipped places describe the deleted floors/priority design**, one
of them a control label at the point of the click:
`console/src/app.tsx:1868` ("the declared floors decide who pauses first");
`docs/components/virtual-keys.md:56-80` ("a key with a tenant reaches **every**
account… a key with no tenant is served last" — it gets a 403);
`docs/components/agent-subscriptions.md:302-306` (borrowing);
`router-core/src/vkey.rs:36-40`, `config/validate.rs:154-155`,
`console/src/api.ts:12`. The branch **adds** this text; it is not leftover.

## High

**F6 — the lease endpoint ignores ownership on an unlabelled pool.** `owned_by`
returns true when `!managed` (`router.rs:485-487`), which is right for routed
traffic and wrong for `lease_for` (`router.rs:500`). Any lease-enabled key —
including one with no tenant — can lease any seat from any unlabelled provider.
That is the default state and all of rollout steps 1-5. Compose with F1: during
rollout, `{"provider":"claude-max"}` returns a random Claude seat's working
refresh token.

**F8 — routing corrupts Claude health and rotation in the optimizer.**
`claudecode/verify.go:46` health-probes through the gateway with the token
discarded; `issues/agent.go:1485-1500` and `workers/manager.go:275-303`
classify CLI error text and call `RotateNextToken`. One gateway 429 or a bad
virtual key benches up to five healthy accounts per run.

## Medium / low

- **F9** `lease_accounts` has no `KeyUpdate` field (`admin.rs:528-547`), is
  hardcoded false in the CLI (`router-bin/src/main.rs:464`) and absent from the
  console type — it cannot be cleared or seen. `lease_account` calls neither
  `meter` nor `vk.admit()`, so leases never reach `/admin/api/requests` and are
  exempt from the key's rate limit. *(The leased document is **not** logged or
  persisted anywhere — checked specifically.)*
- **F10** `rapid-router key create --tenant <typo>` writes straight to the store
  with no check (`main.rs:471`); `check_tenant` exists only in `admin.rs`.
  `virtual-keys.md:80-81` says the CLI validates. `main.rs:123` documents
  `[tenants.<name>]`; the syntax is `tenants = ["…"]`.
- **F11** The console can flip a pool to `managed` with one click, cutting off
  every master-key caller, and never warns — `managed` is passed to
  `CredentialRow` (`app.tsx:564`, props at 1240) and never read.
- **F12** Passthrough changed from `keys.first()` to `admit_any`
  (`proxy.rs:2659`): weighted-random, consumes the rate bucket, can 503 where it
  previously could not. So "unlabelled behaves exactly as today" is not quite
  true. Passthrough also never resolves its `Probe` admission (self-heals after
  one cooldown; pre-existing in kind).
- **F13** `lease_for` filters on `looks_healthy` only — never `is_expired` or
  `wants_refresh`, no TTL, no release, no outstanding-lease record.
- **F14** `lib.rs:705` calls `holding("*")`, which requires `k.models` to
  literally contain `"*"`, so a service whose accounts are model-scoped and
  benched gets 403 "owns nothing" instead of 429 "out of quota".
- **F15** The carried-in foreign work was understated — see REVIEW-BRIEF §10.

## Positions on the open questions

- **Unassigned serves nobody** — keep it, but the silence is the bug: an
  unlabelled account in a labelled pool renders green and serves zero traffic.
  Warn on the row and in `/health`.
- **Loud failure vs degrade** — loud, definitively; degrading re-creates
  borrowing through the back door. But move the noise earlier: warn at config
  load when any provider is `managed` and any key has no tenant, and make the
  console confirm the first label on a shared pool with a count of who gets cut
  off.
- **Prompt caching** — an argument against proxying long multi-turn runs
  generally, and **not** an argument for lending, which trades a measurable cost
  problem for the loss of spend visibility. Measure, then build session affinity
  (the candidate set is already computed in `admit_for`; hash a session key to
  an index within it and fall through when benched). REVIEW-BRIEF §6.1 found the
  CLI already sends `prompt_cache_key`.
- **The migration guide's lease plan** — does not exist. `/v1/accounts/lease` is
  not mentioned in that document once, and §6.1 of it says Codex needs
  `OPENAI_BASE_URL` with "no `auth.json` at all", contradicted by §3 of the same
  document. §6.2 says delete `sanitize.go`, which under a lend design is the one
  file that must survive.
- **The three pre-existing optimizer failures** — not confirmed; no tests were
  run. `go vet ./internal/...` passes. Record the three test names, because an
  unnamed "three pre-existing failures" is not verifiable later. Note
  `go build ./...` fails in `cmd/rapid-optimizer-sandbox` on darwin
  (`syscall.Mount` with no build tag) — pre-existing and unrelated.
- **One refutation of §7.** "A utilization gate was measured wrong because the
  router normalizes load" is correct about a *utilization* gate but does not
  cover a **lease count and TTL**, which is a property of the leases rather than
  of pool traffic. That control is absent rather than rejected.

## Also worth recording

**Master-key traffic can never carry a tenant** — there is no field for it, and
`gateway_auth` sets `ctx.0 = None` on a static key match (`lib.rs:970`). So
"keys first, labels last" is not a preference: every master-key caller must be
fully migrated to a `ck-` key before any account is labelled, with no partial
state available.

**Unverified, needs ten minutes with the real binary:** the optimizer writes
`~/.claude/.credentials.json` (`claudeaccounts/manager.go:153,725`), leaves
`HOME` unchanged for routed spawns, sets no `CLAUDE_CONFIG_DIR`, and ships its
sandbox disabled. If Claude Code prefers that file over `ANTHROPIC_AUTH_TOKEN`,
every routed run silently spends the host login while F8 benches the pool.

`docs/guides/coding-agents.md` was the one document found straightforwardly
honest — dated, versioned, stating what was measured and what failed.
