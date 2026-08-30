# Prompt caching collapses on routed agent runs

*Measured in production 2026-08-29 against `router.rapidclaims.ai`, config v155,
router `b7c1c8d`. This is the item `FINAL-PLAN.md` §15 and `HOW-IT-WORKS.md` §5.2
left open as "unmeasured". It is now measured, and it is real.*

---

## The one-paragraph version

Choosing an account per request, which is what makes ownership enforceable, also
scatters one conversation across a service's whole pool. Prompt caching is
per-account, so a conversation that lands on a different seat every turn finds
nothing cached and pays full price for a prefix it has already sent. A real
routed optimizer run measured **5.0% cache against a realistic ceiling of ~90%** —
roughly **five times** the input tokens it needed. litellm, on the same gateway,
gets 29.9%, which is why nobody noticed. The fix is session affinity, it is a
contained change in one function, and it does not weaken ownership at all.

---

## What was measured

One real optimizer issue (`issue-00374bc1`), Codex agent, routed, two turns of
genuine tool use — shell, file read, file write, `rg`, and an MCP call. Thirteen
requests on the `optimizer` virtual key, all `200`, all `endpoint=responses`,
`attempts=1`, served by real `codex_subscription` seats.

```
req   input_tok   cached   cache%
  1      28,861        0     0.0%
  2      29,202        0     0.0%
  3      29,511    3,840    13.0%
  4      29,800        0     0.0%
  5      29,913   12,032    40.2%
  6      30,018        0     0.0%
  7      30,832        0     0.0%
  8      31,001        0     0.0%
  9      31,082        0     0.0%
 10      31,352        0     0.0%
 11      31,427    3,840    12.2%
 12      31,669        0     0.0%
 13      31,977        0     0.0%

total   396,645   19,712     5.0%
```

Three hits in thirteen turns. With twelve seats labelled `optimizer`, random
re-selection predicts about one hit in twelve. The observed rate is what the
design predicts, not an anomaly.

Note what turns 3, 5 and 11 prove: when a turn happens to land back on a seat
that recently served this conversation, the cache is **there and works**. The
key is right and reaches upstream. Only the seat is wrong.

---

## Why it happens

The upstream backend caches the prefix of a request, and **that cache belongs to
the account that made the request**. Each of the 119 Codex seats is a separate
ChatGPT account with a separate cache. Seat A's cache is invisible to seat B.

The old design leased one seat for a whole run, so turn *n* always found turn
*n-1*'s prefix already cached. That property came from **pinning**, not from
lending — a distinction worth holding on to, because everything that was wrong
with the old design came from lending and nothing came from pinning.

`admit_for` picks per request and deliberately spreads load
(`balanced_pick`), so a conversation walks the pool:

```
turn 1  →  seat 4    cold
turn 2  →  seat 11   cold
turn 3  →  seat 4    HIT — seat 4 still holds turn 1's prefix
turn 4  →  seat 7    cold
```

Each turn re-sends ~99% of the previous turn's tokens. Pinned, turns 2–13 would
each cache almost all of it.

---

## Why litellm is not affected, and why that hid this

Measured over the same window, same gateway:

| | litellm | optimizer |
|---|---|---|
| Requests sampled | 1000 | 13 |
| Distinct 200-char prefixes | **26** | **13 — every turn unique** |
| Most common prefix | 467 requests (47%) | 1 request |
| Requests per seat | ~9.4 | ~1 |
| **Cache hit rate** | **29.9%** | **5.0%** |

litellm sends the same chart-coding preamble over and over. Across 1000
requests and 106 `agi` seats, every seat sees that preamble about nine times, so
every seat's cache warms and stays warm. Spreading costs it almost nothing.

The optimizer is the opposite shape. Each turn's prefix belongs to one
conversation and grows every turn — turn 5's prefix exists nowhere else. Across
13 requests and 12 seats, no seat sees the same conversation twice, so no cache
ever warms.

> **Caching survives load-spreading when the repeated text is shared across many
> requests. It dies when the repeated text belongs to one conversation.**

Agent runs are entirely the second kind. That is why a gateway that looks
healthy on 1.5M litellm requests silently breaks the optimizer, and why this
could not have been caught by watching aggregate cache rates.

---

## What it costs

`/admin/api/requests` reports `cost_micro_usd` — for that run, $0.53. **That
figure is an estimate at API list prices and is the wrong unit.** These are
subscription seats. The real currency is each account's **weekly quota**.

So the finding is not "we spent 5× the dollars". It is:

> **Multi-turn agent runs burn roughly five times the seat quota they need.**

Which lands on the exact worry that started this workstream. The feature exists
to stop one service draining the pool; the mechanism it uses to enforce that
makes the draining substantially faster for the one service that runs long
conversations. Worth stating plainly when deciding what to do next.

---

## The fix: session affinity

Pin the *choice of account* to a conversation. The credential stays in the
gateway, ownership is unchanged, nothing is lent. This restores the old design's
caching without restoring anything else about it.

### Everything needed is already present

| | |
|---|---|
| A stable per-conversation id arrives | The Codex CLI sends `prompt_cache_key` on every request |
| It survives the relay | [`codex_relay_body`](crates/router-providers/src/subscription.rs) clones the body and overrides only `model`, `stream`, `store`, and a default `instructions` — the caller's key passes through untouched |
| The candidate set is already correct | [`admit_for`](crates/router-core/src/router.rs) applies `owned_by` first, then calls `balanced_pick` on what survives |

Only the last step changes. Affinity indexes **within** the already
tenant-filtered set, so `owned_by` still gates everything and the ownership
guarantee is untouched.

### The trap: do not hash a derived key

When a caller supplies no `prompt_cache_key`, the router derives one in
[`codex_cache_key`](crates/router-providers/src/subscription.rs):

```rust
hasher.update(model);
hasher.update(instructions);
for tool in req.tools { hasher.update(tool.function.name); }
```

That is a key per **workload shape**, not per conversation. Two unrelated
litellm requests with the same system prompt get the same key. Hash it to a seat
and the prefix covering 47% of litellm's traffic pins ~470 requests onto one
account while 105 sit idle — trading a caching problem for a far worse hot-seat
problem, and very likely exhausting one seat's weekly quota inside an hour.

**Affinity must apply only to a caller-supplied `prompt_cache_key`**, falling
back to `balanced_pick` whenever the key was derived. That one condition is the
difference between fixing the optimizer and breaking litellm.

### Rendezvous hashing, not modulo

`candidates[hash(key) % candidates.len()]` is wrong here. The candidate list
shrinks and grows as seats bench and recover, and modulo reshuffles *every*
conversation whenever the length changes — one seat going down costs the cache
on all of them.

Use highest-random-weight: score each candidate as `hash(key, seat.name)` and
take the max. Removing a seat moves only the conversations that were on it.

### Sketch

```
admit_for(model, tenant, now_ms)
  → admit_for(model, tenant, affinity: Option<&str>, now_ms)

  candidates = eligible(model) ∩ owned_by(tenant) ∩ healthy      // unchanged
  if let Some(key) = affinity:
      pick = argmax over candidates of hash(key, seat.name)       // rendezvous
      if pick.try_admit_request(now): return pick                 // rate ceiling honoured
  balanced_pick(&candidates)                                      // existing path
```

Plus: read the caller-supplied key out of the body in `run_responses` /
`run_chat` before selection, and carry a flag distinguishing supplied from
derived.

### What it costs, honestly

- **Load concentration.** `balanced_pick` spreads on purpose, and seats have
  weekly quotas. Pinning concentrates a heavy conversation on one seat.
  Mitigated by falling through when the seat is benched or over its rate
  ceiling — `admit_for` already steps over rate-limited keys. Net burn should
  still fall, because ~5× fewer uncached tokens are sent.
- **Cache TTL.** Upstream prefix caches expire after minutes of inactivity, so a
  session that idles between turns pays a cold miss regardless. Affinity helps
  during active work, which is where the tokens are.
- **Failover.** When the pinned seat is unavailable the turn falls through and
  loses its cache. Because the hash is deterministic, the conversation returns
  to its seat automatically once it recovers.

None of these argue against the change. They argue for fall-through, which is
wanted anyway.

---

## How to test it

Two cases, in the style of the existing `e2e_vkey_accounts.rs` suite, asserting
against the **recorder's** view of which credential was presented rather than
anything the gateway says about itself:

1. **A supplied key pins.** Drive N turns carrying one `prompt_cache_key`;
   assert the recorder saw exactly one seat, and that it was one of the caller's
   own.
2. **A derived key still spreads.** Drive N requests with no
   `prompt_cache_key` and a shared preamble; assert the recorder saw more than
   one seat.
3. **Fall-through.** Bench the pinned seat mid-run; assert the next turn is
   served by a different seat *of the same service*, and that the conversation
   returns to the original once it recovers.

---

## Implemented — 2026-08-30, branch `session-affinity`

| Change | Where |
|---|---|
| `admit_for` / `admit_key` / `admit_any` take an `affinity: Option<&str>` | `router-core/src/router.rs` |
| `pinned_seat` + `rendezvous_score` — highest random weight over the candidate set, keyed by session and seat *name* | `router-core/src/router.rs` |
| The pin is tried once, then falls through to `balanced_pick` if that seat is over its ceiling | `router-core/src/router.rs` |
| `run_responses` reads the caller's `prompt_cache_key` and passes it | `router-server/src/proxy.rs` |
| Every other call site passes `None` — chat, relay, stream relay, passthrough | `router-server/src/proxy.rs` |

Six unit tests and four end-to-end tests, the latter asserting on the
credential the mock upstream received, because the gateway's own records
cannot say which account served — see the last section.

```
a_conversation_stays_on_one_seat                     one conversation, one seat
different_conversations_still_use_the_whole_pool     no hot seat
traffic_with_no_session_still_spreads                the derived-key guard
a_benched_seat_does_not_strand_its_conversations     falls through, then returns
a_conversation_maps_to_the_same_seat_in_a_fresh_process   survives a restart
a_pin_cannot_cross_a_service_boundary                ownership still wins

e2e: a_conversation_keeps_one_account
     turns_with_no_conversation_still_spread
     separate_conversations_use_separate_accounts
     the_cache_key_still_reaches_upstream
```

Gate: **500 passed, 0 failed**; `cargo fmt --all --check` and
`cargo clippy --workspace --all-targets -- -D warnings` both clean.

**Not yet measured in production.** The unit and e2e tests prove the seat
selection; what they cannot prove is the cache rate a real run gets, because
that depends on the upstream honouring the prefix. Re-run the measurement at
the top of this document after deploying, and compare. The G2 fix (account
and tenant on the usage record) would make that a query rather than an
exercise.

## What this deliberately does not do

It does not reintroduce lending, pin a credential to a client, or change what a
key may reach. It does not make caching a guarantee — it makes it likely, which
is all a routing hint can do.

## The measurement that is still missing

Nobody can currently answer *"which account served this request"*. Usage records
carry `vkey, provider, model, status, tokens` and no account or tenant, and the
router logs nothing per request. So the collapse above is **measured** but its
mechanism is **inferred** — the seat-hopping is what the design predicts and what
the hit pattern matches, but it cannot be read off the logs. Adding `account`
and `tenant` to the usage record is a prerequisite for verifying that this fix
worked.
