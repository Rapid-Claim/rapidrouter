# Migrating the three services onto owned accounts

A runbook. Every phase says what changes, why it cannot break live traffic,
how to check that it didn't, and how to undo it.

**The rule throughout: current requests keep being served.** Nothing here is
worth an outage, and every phase before the last one is reversible in seconds.

---

## The constraint that shapes this

Callers cannot be reconfigured. The services sending traffic today are not
things we can go and edit — so any plan that begins "issue everyone a new key
and update their config" is not a plan.

That collides with how ownership works. An account carries a service name; a
request must carry the same name; they must match. Today every caller presents
the same shared master key, which has **nowhere to write a service name** — it
is a password, not an identity, and it cannot be made into one, because
everyone shares it.

So the moment any account is labelled, every master-key request is refused.
With 139 Codex accounts, three unassigned virtual keys and one shared key, that
is a total outage of the Codex pool.

**The fix is in the router, not in the callers.** A static gateway key learns to
carry a service name. The caller sends the byte-identical request it sends
today; the gateway decides which service it belongs to on arrival.

```toml
[server]
# Today — a bare string still works and means "no service".
auth_keys = ["store.master"]

# After — the same key, now attributed.
auth_keys = [{ key = "store.master", tenant = "agi" }]
```

That one change removes the forced ordering entirely. There is no cutover
moment and no window where anything is refused.

---

## Phase 0 — teach a static key to name a service

**Code change, in the router.** Nothing about production changes yet.

| | |
|---|---|
| Config | `auth_keys` entries accept `{ key, tenant }` as well as a bare string |
| Auth | on a static-key match, the request carries that key's service |
| Docs | the rollout ordering warning is replaced by this mechanism |
| The startup warning added earlier | now fires only for a static key with **no** service on a divided pool — which is the real remaining hazard |

**Why it cannot break anything:** a bare string keeps its current meaning
exactly. A deployment that adds no `tenant` anywhere behaves identically, and
that is the state production is in when this ships.

**Gate:** `make e2e` green, plus a new case — a static key with a service
reaches that service's accounts on a divided pool, and one without a service is
still refused there.

**Rollback:** revert the PR; the config still parses because bare strings never
stopped being valid.

---

## Phase 1 — declare the services, attribute today's traffic

Still no account is labelled. Nothing is reserved. The only change is that the
gateway can now say *who* each request was.

1. Declare the roster:

   ```toml
   tenants = ["agi", "kris", "optimizer"]
   ```

2. Attribute the master key to the service that actually uses it:

   ```toml
   auth_keys = [{ key = "store.master", tenant = "agi" }]
   ```

3. Set a service on the three existing virtual keys — Ashutosh, WNS, litellm —
   in the console dropdown. Pick whichever service each genuinely belongs to;
   `agi` is the default answer for gateway traffic.

**Why it cannot break anything:** `owned_by` returns true unconditionally while
no account is labelled, so a key's service is recorded and not yet enforced.
Every caller reaches every account exactly as before.

**Gate:**
- Success rate and request volume unchanged against the Phase 0 baseline.
- `/admin/api/requests` now shows a service on rows that previously showed none.
- **No row is left without a service.** This is the real gate: an unattributed
  caller here is one that would be refused in Phase 3, and finding it now is
  the entire point of the phase.

**Rollback:** remove the `tenant` from the static key, or set the dropdowns back
to Unassigned. Instant, and nothing depended on it.

**Expect this phase to take the longest.** Not because the edit is hard, but
because watching long enough to be sure every caller has appeared is the work.
A service that only runs nightly will not show up in ten minutes.

---

## Phase 2 — give the optimizer and Kris their own keys

1. Create `optimizer` and `kris` virtual keys, each naming its service.
2. **Optimizer:** set `RAPID_OPTIMIZER_LLM_ROUTER_URL` and
   `RAPID_OPTIMIZER_LLM_ROUTER_KEY`. Claude runs route immediately. Codex needs
   the third variable, `RAPID_OPTIMIZER_LLM_ROUTER_CODEX`, and should wait for
   the real-seat test below.
3. **Kris:** not ready. The change in that repo never runs — it is reachable
   only from a test — and it still sends the vendor token as its bearer. Kris
   stays on its current path until that is fixed, and that is a separate PR.

**Why it cannot break anything:** unsetting the variables restores the old path
byte-for-byte, which is the rollback. The optimizer keeps its account pool
until routed traffic has been watched.

**Gate:** run one real optimizer issue. Confirm on the gateway that the request
arrived, carried the `optimizer` key, and was served — and confirm from the
optimizer side that the run completed normally.

**Rollback:** unset the two variables and restart. One step.

---

## Phase 3 — dedicate accounts

Only now, and only once Phase 1's gate says **no caller is unattributed**.

1. Label ten Codex accounts `optimizer` in the console.
2. Watch. Every other caller keeps working because every other caller now names
   a service and the accounts they use are still unlabelled.

**Why this is the phase to be careful about:** it is the first one that
*enforces* anything. If any caller was missed in Phase 1, this is where it
fails, and it fails as a 403 rather than a slowdown.

**Gate:**
- The optimizer's requests are served only by the ten labelled accounts.
- Every other service's success rate is unchanged.
- No 403 with "owns no account" in the log for anyone.

**Rollback:** set those ten accounts back to Unassigned. The pool becomes
undivided and everything reverts. Seconds, through the console.

---

## Phase 4 — the real-seat test, before Codex routing becomes the default

Everything proven so far used a stand-in for the vendor. One real Codex
subscription seat behind the gateway, one real optimizer issue **with tools**,
and a comparison of token spend against an unrouted run of the same issue.

That last one matters: prompt caching is per account, and the gateway picks per
request. A long run may pay full price every turn. If it does, that is an
argument for pinning a conversation to one account — the CLI already sends
`prompt_cache_key` — not for abandoning routing.

---

## Adding a fourth service later

The design is meant to take one. In order:

1. Add its name to `tenants`.
2. Give it a key naming that service — or, if it is another caller we cannot
   reconfigure, its own static key with a `tenant`.
3. Move accounts to it from whoever has spare, one dropdown at a time.

No code change, no restart, no cutover. The only rule that stays true forever:
**a caller that names no service reaches nothing on a divided pool**, so every
new caller needs a name before the pool it wants is divided.

---

## What this runbook does not fix

- **No console path to declare a service.** `tenants = [...]` is config-only, so
  the dropdowns stay empty until someone edits the gateway config. The Service
  column ships and looks broken until then. Worth fixing; not a blocker.
- **Kris.** Phase 2 says why. It needs its own PR.
- **Per-service usage reporting.** `/admin/api/requests` shows the key that
  served each request, but nothing aggregates spend per service yet.

---

## Verification, at every phase

The same three checks, before and after each change:

```bash
# 1. is it up and has it stayed up
systemctl show rapid-router.service -p ActiveState -p NRestarts

# 2. has anything started failing
sudo journalctl -u rapid-router.service --since "10 min ago" | grep -ciE "ERROR|panic"

# 3. is it serving
curl -s -o /dev/null -w '%{http_code}' https://router.rapidclaims.ai/v1/models
```

`NRestarts` climbing is the signal that matters most — it means the process is
dying and being restarted, which no config edit here should ever cause.

Baseline taken 2026-08-29, after the deploy of the ownership change:
active, **0 restarts**, **0 error lines** in two hours, seat maintenance
renewing normally, and `/passthrough/` — the one behaviour that deploy altered
— **completely unused**, so that change is inert in practice.

---

# What actually happened — 2026-08-29

The migration ran. This section is the record, including the four things the
plan above got wrong, because those are the parts worth reading.

## Where it ended up

```
services: agi, kris, optimizer
codex     managed=true    optimizer 12 · agi 107
claude    managed=false   undivided
openai    managed=false   undivided
keys      litellm · WNS · Ashutosh → agi      optimizer → optimizer
```

Proven with a real optimizer issue: the agent's `CODEX_HOME` carried the
gateway's `model_provider` block and **no `auth.json`**; two requests reached
the gateway on the optimizer's key (`gpt-5.6-sol`, both `200`, the second
11,982 input tokens); the agent replied. No local seat was touched.

## What the plan got wrong

**1. `RAPID_MASTER_KEY` is not the request-auth key.** It is the *store sealing*
key — 32 bytes of base64 that encrypt control-plane secrets and derive session
keys. The access notes describing it as request auth are wrong, and that error
hid the next one.

**2. There is no request auth at all.** `server.auth_keys` is empty and
`require_auth` is unset, so the gate is off:

```rust
let gate_active = !config.server.auth_keys.is_empty() || config.server.require_auth;
```

`https://router.rapidclaims.ai/v1/chat/completions` answered **200 to an
unauthenticated request from the open internet.** Four source IPs have ever
used the data plane, all internal, so it has not been found — but it is open,
and the accounts behind it are paid subscriptions. The "12,406 master-key
requests" in the earlier analysis were in fact *anonymous* requests.

**3. Almost nothing needed migrating.** litellm, WNS and Ashutosh were already
virtual keys; they only needed a service set, server-side, with no caller
change. 99.2% of all traffic is litellm alone. The static-key work from
`static-key-service` was still worth having — anonymous callers exist and
cannot be reconfigured — but the feared mass cutover did not.

**4. Dividing a pool strands every account left unlabelled.** Not 10 of 117 —
**all 117** had to be labelled, or the other 107 would have served nobody and
taken litellm's 1.5M requests with them. This is the single most important
correction and it is easy to get wrong, because "give the optimizer ten" sounds
like the rest keep working.

## The order that worked

Each step verified before the next; health 200 and zero restarts throughout.

1. Declare `tenants` — config v132. Nothing enforced.
2. Set the service on the three existing keys, via the admin API.
3. Label **all 117 codex accounts in one atomic import** — v137, 10 optimizer /
   107 agi. One commit, so there was never an instant where `agi` held a single
   account absorbing litellm's traffic. Doing this as 117 API calls would have
   produced exactly that.
4. Point the optimizer at the gateway; restart with no agent runs in flight.
5. Move the optimizer's two own accounts into the gateway, labelled
   `optimizer` — hence 12.

## Gaps this left, worst first

1. **The data plane is open.** Codex is now incidentally protected by the
   ownership rule; **Claude and OpenAI are not.**
2. **Usage records carry no account and no service** — `vkey, provider, model,
   status, tokens` only. "Which account served this" is unanswerable and
   per-service spend can only be inferred from the key. That undermines the
   reason for the migration and should be next.
3. **An account can exist in two systems at once.** The two moved credentials
   still exist in the optimizer's local store. Routing means it will not use
   them, but unset the variables and both processes would refresh the same
   OAuth token — the credential-collapse bug. Remove them there.
4. **Accounts are keyed by name, identity is the email.** Adding
   `sofia.renner@shadowagi.com` failed on a duplicate *name* against a
   different login of the same person. It should key on, or warn about, email.
5. **CLI-created keys lag one store-refresh interval**; admin-API ones apply at
   once. The symptom is a puzzling `401` on a key you just made.
6. **No console path to declare a service** — `tenants` is config-only, so the
   Service column looks broken until someone edits the gateway config.
