# Independent review — service-owned accounts, live

*2026-08-30. An independent read of the shipped feature (`rapidrouter` PRs #26–#31,
`rapid-optimizer` PRs #182–#183) **against production**, not against the branch:
router `b7c1c8d`, config v155, optimizer routed with Codex enabled. Supersedes
nothing; `REVIEW-FINDINGS.md` reviewed the branch before merge, this reviews what
is actually running.*

**Verdict: the design holds and the end-to-end path works. Three gaps are
serious enough to fix before this is called done, and one of them is not caused
by this work but is now load-bearing on it.**

---

## 0 · What was proven, first

The open item — *"nothing has run against a real Codex subscription seat with
tools"* — is now closed. A real optimizer issue (`issue-00374bc1`, WNS Demo org),
Codex agent, routed, two turns, forced through genuine tool use.

| Check | Result |
|---|---|
| `CODEX_HOME` config | `model_provider` written **first**, before the MCP tables; `wire_api = "responses"` |
| Credential on disk | **no `auth.json`, no `credentials.json`** — before turn 1 and still absent after turn 2 on the reused session home |
| Tools | shell, file read, file write/edit, `rg`, and an MCP call — all succeeded |
| Real work | 227 dev / 223 prod workflows enumerated, a site `tests.yaml` read, 1250 files matched, a new file written and read back |
| Failure honesty | agent hit `python: command not found` and reported it verbatim |
| Constraints | `git status` showed only the one permitted new file; no PR |
| Gateway | 13 requests on the `optimizer` key, **all 200**, `endpoint=responses`, `attempts=1`, real `codex_subscription` seats |
| Local pool | `routed: true`; no seat leased, no account benched |

The ownership gate was also verified live, in production, rather than against a
fixture:

```
anonymous → codex   (managed)    403  "this key owns no account on provider `codex`…"
anonymous → claude  (undivided)  200  served
```

Both are exactly what the design specifies. `owned_by` works.

**For the record on what the earlier proof was worth:** the previous "Router
migration smoke test" (`issue-9e2074aa`) instructed the agent *"do not change any
files, do not run tests, do not inspect the repository. Reply with one short
sentence."* It established transport and nothing else. `HOW-IT-WORKS.md`'s
"Proven with a real optimizer issue" reads stronger than what was done.

---

## Blockers

### G1 — the data plane is open, and ownership is now accidentally load-bearing on it

`[server]` in live config v155 has **no `auth_keys` and `require_auth` unset**, so
the gate in `lib.rs` is inactive:

```rust
let gate_active = !config.server.auth_keys.is_empty() || config.server.require_auth;
```

The nginx vhost is a bare public proxy — no `allow`, no `deny`, no `auth_*`, no
`limit_req`:

```
/etc/nginx/sites-available/router.rapidclaims.ai
  listen 443 ssl;  server_name router.rapidclaims.ai;
  location / { proxy_pass http://rapid_router; }
```

Every link in the chain is verified, not inferred:

| Link | Verified |
|---|---|
| Public DNS | `router.rapidclaims.ai` → `34.233.227.213` (resolved via `8.8.8.8`) |
| Security group | `sg-04398f347a42a9fc4` allows `0.0.0.0/0` inbound on **80 and 443** |
| Edge | nginx `location / { proxy_pass http://rapid_router; }` — no auth, no allowlist, no rate limit |
| Gateway gate | `auth_keys` empty and `require_auth` unset → `gate_active == false` |
| Behaviour | from a different host with no credentials: `/v1/models` → `200`; `/v1/chat/completions` on codex → `403` from the **ownership** rule only. On the router host, `claude/claude-opus-5` → **`200`, spending a real seat** |

So Codex is protected only because it happens to be divided.

**The sharpest exposure is not the subscription pools — it is the `openai`
provider.** One account, `main`, `base_url = https://api.openai.com/v1`,
`models = []` (all models), no tenant, and per G6 no budget. A subscription seat
has a weekly ceiling that bounds the damage; a metered API key has none. Any
anonymous caller who reaches the hostname can bill that key without limit.

This predates the service-accounts work, but that work is now the only thing
standing in front of 119 subscription seats, and nothing at all stands in front
of the metered key.

*Fix:* set `auth_keys` — the `{ key, tenant }` mechanism from PR #27 already
exists and was built for exactly the callers that cannot be reconfigured. An
nginx allowlist, or tightening the security group off `0.0.0.0/0`, is a same-day
stopgap. Setting a budget on the `openai` account bounds the worst case
independently.

### G2 — usage records carry no account and no service

Confirmed at the storage layer, not just the API. Every record is:

```
ts, request_id, endpoint, requested, provider, model, vkey,
status, stream, input_tokens, output_tokens, cached_tokens,
cost_micro_usd, latency_ms, overhead_us, attempts
```

No seat. No tenant. The router also logs nothing per request — **77 journal lines
in 24 hours** against roughly 1.5M requests.

Consequences, in order of how much they matter:

1. *"Which account served this request"* is unanswerable. That is the question
   the whole ownership model is about.
2. Per-service spend is only inferable by joining `vkey` → tenant, a mapping
   that is mutable and not snapshotted. Relabel a key and history silently
   re-attributes.
3. It blocks diagnosis of G3 — see that finding.

`HOW-IT-WORKS.md` says *"the gateway sees every request, so spend is attributable
per service for the first time."* The gateway does see them; it does not record
who they were for.

*Fix:* add `account` and `tenant` to the usage record. Two fields, both already
in scope at the point the record is written.

### G3 — prompt caching collapses on routed agent runs

Measured: **5.0% cache hit rate** across 396,645 input tokens on a real routed
run, against a realistic ceiling near 90%. Roughly **five times** the seat quota
needed. litellm on the same gateway gets 29.9%, which is why it went unnoticed.

Full measurement, mechanism, the derived-key trap, and the session-affinity fix
are in **[`PROMPT-CACHE-COLLAPSE.md`](PROMPT-CACHE-COLLAPSE.md)**.

The point worth repeating here: the currency is **weekly seat quota**, not
dollars, so this makes the pool drain roughly five times faster for the one
service that runs long conversations — which is the exact problem this workstream
exists to solve.

---

## High

### G4 — the console's bulk assign re-introduces the hazard it says it prevents

[`console/src/app.tsx`](console/src/app.tsx) states the danger precisely:

> *"done sequentially there is a window where the service that owns most of the
> pool holds only the few already moved — so the bulk path is the safe one as
> well as the quick one."*

The code beneath it:

```js
for (const name of names) {
  try { await api.setAccountTenant(provider().name, name, tenant); }
  catch { failed += 1; }
}
```

A sequential `await` loop. 117 accounts is 117 API calls and 117 config version
bumps, each with its own optimistic-concurrency check against `commit_document`.
On partial failure it reports *"X of N could not be changed"* and leaves the pool
**mixed** — some labelled, some stranded and serving nobody.

It removed the 117 confirmations, not the window. The live migration was done as
*one atomic import* specifically to avoid this, and the record calls that "the
single most important correction". The console now offers the shape that record
warns against.

`set_account_tenant`'s own comment — *"One field on one account, so a move cannot
be half-applied"* — is true of one account and false of the operation the button
performs.

*Fix:* a bulk tenant endpoint. `/providers/{name}/keys/bulk` already exists and
`deleteProviderKeys` already uses it for atomic bulk delete; the pattern is in
the codebase, it just was not applied to the one operation whose own comment says
atomicity matters.

### G5 — a stranded account in production, and nothing warns

Live state of the `codex` pool: 119 accounts, `managed=true`, **`optimizer` 12 ·
`agi` 106 · `UNASSIGNED` 1**.

- `anouk-vandermeer-2` carries no tenant in a divided pool. It reports
  `healthy` / `ready`, is in quota, and **serves nobody**.
- `sofia-renner-2` (tenant `agi`) has its breaker `open`.

`FINAL-PLAN.md` item 4 — *"warn at config load when any provider is `managed`
while any key has no tenant, so the first symptom is a startup line rather than
production 403s"* — **was never implemented.** `validate.rs` validates undeclared
tenant *names* thoroughly (on accounts, on keys, and on static keys), but has no
managed-pool-with-untenanted-key check.

The console does surface it — the row renders *"unassigned — serves nobody"*, and
the first-label confirmation landed — but only if somebody is looking at that
screen. `/health` returns `{"status":"ok"}` and says nothing.

*Fix:* the config-load warning, and label or remove `anouk-vandermeer-2`.
Consider surfacing stranded-account count in `/health`, as the earlier review
suggested.

### G6 — no budgets anywhere; the plan's own policy was not implemented

All four virtual keys: `budget=null, rate=null`.

`FINAL-PLAN.md`'s central argument is **"budget everything, label almost
nothing"** — a label guarantees access but strands capacity, a budget caps
consumption and strands nothing. Rollout step 4 was *"set a weekly budget per
key."*

What shipped is the inverse: **118 labels, zero budgets.** The consumption worry
that started the project is still entirely unaddressed; only *access* was
partitioned. Combined with G3, the one service most able to drain the pool is
both uncapped and burning ~5× its necessary quota.

*Fix:* set weekly budgets per key. `vk_gate` already enforces them; this is
configuration, not code.

---

## Medium / low

### G7 — the Claude pool is Kris-only by design — withdrawn as a finding

Originally filed because the `claude` provider holds 2 undivided accounts while
the optimizer routes Claude runs with no local fallback.

**Withdrawn 2026-08-30 on the product owner's answer:** the Claude pool is
intentionally reserved for Kris, that path is being worked on separately, and the
optimizer is not expected to depend on it. Volume supports this — agent providers
across 185 issues are **codex 179 / claudecode 6**.

One part survives, and it belongs to G1 rather than here: those two seats are
still spendable by any anonymous caller until the data plane is closed. Reserving
them for Kris is a policy decision the gateway currently cannot enforce, because
enforcement requires the caller to be identified and today no caller has to be.

### G8 — `kris` is declared but empty, and caret-agent is still untouched

`tenants = ["agi", "kris", "optimizer"]`; `kris` holds 0 accounts and 0 keys.
caret-agent still has findings F2 (the change is only reachable from a test) and
F3 (it ships the vendor OAuth token to the gateway as its bearer) open. Kris
remains on its old path, which is safe but means the roster advertises a service
that does not exist yet.

---

## What was checked and found correct

Stated so this reads as a review and not a list of complaints. All verified in
the shipped code:

- **`owned_by` is genuinely a single gate.** Every credential-selection path
  routes through it, and `tenant` is never read from a header or body — the only
  sources are `vk.def.tenant` and config, so a caller cannot assert a service.
- **F14 fixed.** `holding()` now takes the real `route.upstream_model`, not
  `"*"`, and the 403/429/no-capacity classification is three distinct causes with
  three distinct answers.
- **`attempt_budget` is tenant-scoped.** It uses
  `healthy_key_count(model, tenant, now)`, so a two-seat service does not inherit
  a ninety-seat retry budget.
- **F8 fixed, and well.** `issues/agent.go` guards rotation on `!routedClaude`
  with a comment explaining that benching a pool account for a gateway error is
  "wrong twice".
- **F10 fixed.** `rapid-router key create --tenant` now validates against the
  declared roster with a helpful message.
- **F11 fixed.** The console confirms before the first label divides a pool, and
  marks unassigned rows in a managed pool.
- **`delete_tenant` is properly guarded** — it refuses while an account, a
  virtual key, or a static key still names the service, and says which.
- **`leases` is a cumulative weight, not a leaked gauge.** It is rebased on
  config reload so a new seat is not flooded. Correct, though the admin API field
  name invites misreading it as "in flight".

---

## The pattern worth naming

Three of the findings above (G3's "unmeasured", G4's comment-versus-loop, G6's
policy-versus-config) share a shape: **the documentation is ahead of the code,
and it is persuasive enough to read as completion.** The prose in this repo is
unusually good, which is precisely why it hides gaps — a comment that explains
why something is safe reads like evidence that it is.

Suggested rule for anyone reviewing this codebase next: **where a comment
explains why something is safe, check that the code below it implements the
safety.** All three were found that way.

---

## Suggested order

1. **G1** — close the data plane. Only genuinely urgent item.
2. **G2** — add `account` and `tenant` to the usage record. Small, and it
   unblocks verifying G3.
3. **G3** — session affinity, per `PROMPT-CACHE-COLLAPSE.md`.
4. **G4** — bulk tenant endpoint.
5. **G5** — config-load warning; fix the stranded account.
6. **G6** — set weekly budgets.
7. **G7 / G8** — Claude seat policy; caret-agent as its own PR.
