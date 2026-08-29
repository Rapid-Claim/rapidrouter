# How account routing used to work, and how it works now

Written to be read start to finish by someone who did not build the original.
No prior context assumed.

---

## The one-paragraph version

We own about 139 Codex and Claude **subscription accounts**. Three services
want to spend them. Until now there was no way to say which accounts belong to
which service, so any one of them could drain the lot — and the way each
service *got* an account was to be handed the raw login file and use it
directly. The change makes each account carry the name of the service that owns
it, and moves the services from "here is a login file, go use it" to "send your
request to the gateway and it will pick one of your accounts".

---

## Part 1 — How it used to work

### The thing being shared

A **subscription account** is a ChatGPT or Claude login that someone paid a
monthly fee for. It is not an API key. It is an OAuth login: an access token
that expires in hours, a refresh token used to get a new one, and a weekly
usage quota. When an agent CLI runs "as" that account, it is doing the same
thing a human would by logging in.

For Codex, that login lives in a file called `auth.json`. For Claude Code, it
is a token in an environment variable. This matters, because the whole old
design was about **moving those files and tokens around**.

### The three places accounts lived

**1. The AGI Gateway.** A Python service holding the accounts, refreshing their
tokens, and serving application traffic. The credentials sat in
`/etc/agi-gateway`, owned by root, mode `0600`.

**2. A mirror on disk.** The optimizer runs on the same machine but as an
unprivileged user, so it could not read that directory. The obvious fix —
granting group access — does not survive, and the script that solves it
explains why better than I can paraphrase:

> the gateway rewrites a seat's auth.json on every OAuth refresh via
> `tempfile.mkstemp` + `os.replace`, so the file comes back `0600 root:root`
> and any group grant silently decays. Because a seat only refreshes every few
> days, that decay shows up as one seat mysteriously dying at a time, days
> after anyone touched anything.

So `scripts/agi-pool-sync.sh` runs as root on a timer and **copies** what the
optimizer needs into a directory it owns: the seat list, each seat's
`auth.json`, and only the `CLAUDE_CODE_OAUTH_TOKEN_*` lines from the gateway's
env file — deliberately not the whole file, which also holds AWS keys and a
database password. It is a one-way copy. The gateway owns the originals and is
the only thing allowed to refresh them; writing back would corrupt its refresh
logic.

**3. The optimizer's own accounts.** Separately, accounts you add in the
optimizer's Settings page. `internal/app/agipool` can be set to
`local-only` (the default — never touch the shared pool), `local-first` (prefer
your own, fall back to the shared pool), or `gateway-only` (ignore your own).

### What actually happened on one agent run

This is the part you were asking about — the JSON-file shuffling.

1. A run starts and asks the pool for a **seat**.
2. The pool picks one and marks it in use, so two runs do not collide.
3. It hands back a credential:
   - **Codex** → the raw contents of that seat's `auth.json`
   - **Claude** → that seat's OAuth token
4. The runner builds a private `CODEX_HOME` directory for the run and **writes
   the `auth.json` into it**. For Claude it puts the token in
   `CLAUDE_CODE_OAUTH_TOKEN` in the child's environment.
5. It spawns the real `codex` or `claude` binary pointed at that home.
6. **The CLI talks straight to OpenAI or Anthropic**, authenticating as that
   account. Nothing of ours is in the middle.
7. When the run ends, the seat is marked healthy (or benched if it hit a rate
   limit) and released.

So your description was right: the system worked by copying login files into
the right place and letting the vendor's own CLI use them.

### Why that was a reasonable design

Worth saying plainly, because we are replacing it: it works, it needs no
protocol translation, tool calls and streaming behave exactly as the vendor
intended, and prompt caching is perfect because one run stays on one account
the whole time.

### What was wrong with it

**Nobody owns anything.** Any run can take any seat. A long batch job and a
Slack bot that a human is waiting on draw from the same pot, first come first
served. Nothing can be reserved.

**Nothing is attributable.** Spend happens at the vendor, so there is no record
of which service spent what. Everything reaching the gateway also used a single
shared master key, which names no service.

**The mirror went stale.** rapid-router replaced the Python gateway on
2026-08-18 and that container stopped. The mirror still points at the retired
directory: **77 accounts where the router now serves 139**. Sixty-two accounts
were simply invisible.

**A credential can land in the wrong place.** The CLI refreshes tokens in place,
inside the run's `CODEX_HOME`. If that home is reused, or the rotated file gets
read back and filed under the wrong account, one account's login ends up saved
as another's. The code calls this credential collapse, and it had happened.

---

## Part 2 — How it works now

### The rule, in one line

**An account carries the name of the service that owns it. A key carries the
name of the service it is. They must match.**

```toml
# an account, in the gateway's config
{ name = "seat-07", value = "file:/…/auth.json", tenant = "optimizer" }

# a virtual key, issued to a service
tenant = "optimizer"
```

Same name → may spend it. Different name → refused. That is the entire
mechanism. No borrowing, no priority order, no "use the spare capacity if
you're idle". Moving an account between services is editing one word.

The enforcement is five lines in `router.rs`, and every path that picks a
credential goes through it.

### The part that keeps this safe to merge

**An account nobody has labelled is shared exactly as before.** A provider where
no account has a `tenant` behaves identically to today — every key reaches every
account. So merging this changes nothing until somebody labels something.

The flip side, which you must know before rolling out: once *any* account in a
pool is labelled, a key with **no** service name reaches nothing there. It gets
a `403`, not "last in line". That is deliberate — falling back to the unlabelled
accounts would quietly recreate the free-for-all — but it forces an ordering:

> **Every caller must be moved onto its own `ck-…` key before the first account
> is labelled.** Traffic on the shared master key can never carry a service
> name; there is no field for it.

### What a run looks like now

1. A run starts. **It asks for no seat.** No credential is fetched, copied or
   written anywhere.
2. The runner points the CLI at the gateway and gives it a **virtual key** —
   `ck-…` — which identifies the *service*, not an account.
3. The CLI sends its request to the gateway.
4. The gateway reads the key, sees which service it is, and picks one of **that
   service's** accounts.
5. The gateway makes the vendor call itself, using that account's credential,
   which never leaves the gateway.
6. The response streams back to the CLI.

The account is chosen **per request** rather than per run, so a benched seat can
be routed around mid-run. And the gateway sees every request, so spend is
attributable per service for the first time.

### The one genuinely awkward detail

`OPENAI_BASE_URL` — the obvious way to point the Codex CLI somewhere — **does
not work.** codex-cli 0.146.0 ignores it, with no warning and no error. It talks
to `api.openai.com` while looking configured. We found this the expensive way:
a run that appeared routed spent a real account.

The only thing that redirects it is a config file written into the run's
`CODEX_HOME`:

```toml
model_provider = "rapidrouter"
[model_providers.rapidrouter]
base_url = "http://<gateway>/v1"
env_key  = "RAPID_ROUTER_KEY"
wire_api = "responses"     # without this the CLI opens a transport we don't serve
```

Claude Code is well behaved and does respect `ANTHROPIC_BASE_URL`.

---

## Part 3 — Every change, plainly

### rapid-router — [PR #26](https://github.com/Rapid-Claim/rapidrouter/pull/26)

| What | Why |
|---|---|
| `tenants = [...]` in config; a `tenant` on each account and each key | Somewhere to declare services, so a typo is a startup error rather than a key that silently owns nothing |
| `owned_by()` in `router.rs`, applied wherever a credential is chosen | The rule itself |
| Passthrough switched from "always the first key" to proper selection | It was the one path that ignored ownership |
| Admin API: move an account, see who owns what | So reallocating is a click, not a config edit and restart |
| Console: a Service column on accounts, a Service picker and an Accounts drawer on keys | Same, with a human looking at it |
| **A lease endpoint, added and then removed in the same PR** | See below — worth understanding |
| Corrected the console label and three docs | They described "floors" and "borrowing", a design that was built, measured and deleted. The console text sat directly under the control, telling operators the opposite of what the code does |
| `make e2e` | See Part 4 |

**About the lease endpoint.** Partway through, I added
`POST /v1/accounts/lease` — the gateway hands a service one of its own
credentials to use directly. It existed on the belief that the Codex CLI could
not be pointed at the gateway at all. Once that turned out to be false, lending
lost on every count: no spend visibility, one account pinned per run, and it
handed live credential material out over HTTP behind a guard that did not
actually work. It is gone. The commits are both in the PR on purpose, so the
reasoning is on the record.

### rapid-optimizer — [PR #182](https://github.com/Rapid-Claim/rapid-optimizer/pull/182)

**Off by default.** Unset two environment variables and every run leases a seat
exactly as it does today. That is the rollback, and it is why the old pool is
not deleted in the same change.

| What | Why |
|---|---|
| Point Codex with a `config.toml`, not `OPENAI_BASE_URL` | The env var does nothing, silently (above) |
| A routed run takes **no seat** | Every spawn path leased first and refused on an empty credential, so routing did nothing at all — a fully routed deployment was still gated by the stale 77-account mirror |
| A routed run touches none of the pool's bookkeeping | A `429` from *our gateway* must not bench a pool account that never served the request |
| Clear `auth.json` from a reused session home, and never harvest one from a routed run | Otherwise the first routed run of an old session authenticates with yesterday's seat — and could file its credential under a different account's name |
| Real end-to-end tests | The unit tests were all green the entire time `OPENAI_BASE_URL` was sending runs to the vendor |

### caret-agent (Kris) — **not done**

Started, but the review found the change never actually runs — it is only
reachable from a test — and that it still sends the real Anthropic token to the
gateway as its bearer. Not raised as a PR. Kris is unaffected for now.

---

## Part 4 — How this is tested

Run `make e2e` in the rapidrouter checkout. It starts a real gateway against a
stand-in for the vendor that **records what it receives**, then makes real HTTP
requests and checks the answers.

The important design point: the stand-in logs the credential the gateway
presented. So "the optimizer never touched Kris's account" is established from
the *vendor's* side, not from anything the gateway says about itself.

Fourteen checks, including the two the design turns on that had no test before:
moving an account between services takes effect on a running gateway, and a
vendor `429` does not fall back onto another service's account.

`make e2e-hold` leaves it running and prints the commands to drive it with the
real `codex` and `claude` binaries.

---

## Part 5 — What to push back on

I would rather you argue with these than accept them.

**1. Partitioning might be the wrong default.** The worry that started this was
"what if one service consumes everything". A label answers that by walling off
capacity — which also means Kris's two accounts sit idle overnight while the
optimizer is throttled. A subscription seat's real limit is a *weekly quota*, and
the gateway already enforces per-key budgets. A weekly token budget per service
matches the real constraint and strands nothing.

My suggestion: **budget everything, label almost nothing.** Reserve accounts only
where a human is waiting on the answer. The code already allows this, because
unlabelled accounts stay shared — it is a policy call, not a code change.

**2. Prompt caching is unmeasured, and it is a real cost.** Caching is per
account. The old design pinned one account per run, so a fifty-turn run cached
perfectly. The new one picks per request and deliberately spreads load, so the
same run may pay full price every turn. Nobody has measured it. This is the
strongest argument *for* the old design and it should be settled with numbers
before routing becomes the default.

**3. Nothing has touched a real subscription account yet.** Every test uses a
stand-in for the vendor. The gateway → `chatgpt.com` leg is the same code
production has run since 18 August, but no test here has put a real seat behind
it, and no run has used tools — which is most of what the optimizer does. One
real account and one real issue would close it.

**4. The rollout order is unforgiving.** Because master-key traffic cannot carry
a service name, the first label you apply cuts off every caller still using it.
There is no halfway state.

---

## Part 6 — Where things stand

| | |
|---|---|
| rapid-router | [PR #26](https://github.com/Rapid-Claim/rapidrouter/pull/26) — 14 commits, 479 tests, `make e2e` green |
| rapid-optimizer | [PR #182](https://github.com/Rapid-Claim/rapid-optimizer/pull/182) — 7 commits, off by default |
| Kris / caret-agent | not started properly; see the review findings |
| Deployed anywhere | no |

Further reading, in order of usefulness: `FINAL-PLAN.md` (the plan and the
policy argument), `REVIEW-FINDINGS.md` (an independent review of this branch,
fifteen findings), `REVIEW-BRIEF.md` (what a cold reader needs, including the
designs already tried and rejected so they do not get proposed again).
