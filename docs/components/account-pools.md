# Account Pools & Services

**Status: implemented in the gateway; consumers not yet migrated.** The
labelling and enforcement ship (§1–§5). The optimizer and Caret still hold
their own credentials and have not been pointed here yet — that migration is
§7–§9.

One collective place for every provider account, and one label that says
which service may spend each one.

---

## 1 · The model

Four nouns:

- **Account** — one credential the gateway holds. A Codex seat, an API key.
- **Pool** — all the accounts of one provider.
- **Service** — something that sends traffic: the AGI gateway, Kris, the
  optimizer.
- **Virtual key** — the `ck-…` credential a service presents. Many keys may
  belong to one service.

And one rule:

> An account carries the name of the service it belongs to. A key carries
> the name of the service it is. A request may spend the accounts whose
> label matches, and no others.

There is no sharing, no borrowing, no priority, no arithmetic. An account
belongs to exactly one service, or to none.

## 2 · Configuration

```toml
tenants = ["kris", "agi", "optimizer"]      # the valid service names

[providers.codex]
type = "codex_subscription"
keys = [
  { name = "seat-01", value = "file:/var/lib/rapidrouter/credentials/codex_01.json", tenant = "kris"      },
  { name = "seat-02", value = "file:…",                                             tenant = "optimizer" },
  { name = "seat-03", value = "file:…",                                             tenant = "agi"       },
  { name = "seat-04", value = "file:…"                                                                   },
]

[[virtual_keys]]
name   = "optimizer-runner"
tenant = "optimizer"
```

`seat-04` is **unassigned**: it belongs to nobody and serves nobody until
someone labels it. New accounts arrive that way on purpose — nothing starts
spending capacity by accident.

The owner lives **on the account**, not in a separate list, so an account
cannot be claimed by two services and cannot be forgotten by all of them.
Moving one is an edit to a single word.

## 3 · Enforcement

Five lines in `ProviderRuntime::owned_by`, applied where a credential is
selected:

```rust
if !self.managed { return true; }              // no labels in this pool: shared, as before
match (key.tenant.as_deref(), tenant) {
    (Some(owner), Some(caller)) => owner == caller,
    _ => false,                                // unassigned account, or key with no service
}
```

A pool where **no** account carries a label is shared by everyone —
which is every provider nobody has divided up, unchanged from before this
existed.

Everything downstream is untouched: health filtering, least-used selection,
per-account rate ceilings, breakers, fallback chains. Labels only decide
which accounts are candidates. Every path that spends an account goes
through this one check, including the `/passthrough/…` relay.

## 4 · What a refused caller is told

| Situation | Answer |
|---|---|
| The service owns no account here | `403` — *service `optimizer` owns no account on provider `codex` that can serve model `gpt-5.6`* |
| It owns accounts, all out of quota | `429` — *service `optimizer` has no account left on provider `codex`: all 10 of its accounts are out of quota* |

Two different problems, two different next moves: fix the configuration, or
move an account across.

## 5 · Moving an account

One call, one field. The source is implied — the account already knows who
owns it:

```
PUT /admin/api/providers/codex/keys/seat-04/tenant   { "tenant": "optimizer" }
```

`{"tenant": null}` unassigns it. Applied through the store, live on every
node, no restart, and no credential moves anywhere.

The console surface (not yet built) is a table per provider:

| Service | Accounts | Usable | Out of quota | |
|---|---:|---:|---:|---|
| kris | 2 | 2 | 0 | **+** |
| optimizer | 10 | 0 | 10 | **+** |
| agi | 105 | 98 | 7 | **+** |
| *unassigned* | 0 | — | — | |

**+** asks which service to take one from, picks that service's freshest
account, and relabels it. `holding()` already returns the two counts each
row needs.

## 6 · Validation

Refused when the config loads:

- An account labelled with a service that is not in `tenants`.
- A key labelled with a service that is not in `tenants`.
- A duplicate name in `tenants`.

A typo would otherwise leave an account owned by nobody, or a key with no
accounts at all — both look fine until the pool is busy.

---

# The migration

Today, only some traffic reaches the gateway as HTTP. The optimizer and
Caret hold provider credentials themselves and hand them to agent CLIs. Until
that changes, labelling accounts governs nothing they do.

## 7 · Where each consumer stands

| | How it gets an account today | What it spawns |
|---|---|---|
| **AGI gateway traffic** | HTTP to rapid-router `:8091` with the shared `RAPID_MASTER_KEY` | — |
| **Optimizer** | `internal/app/agipool` leases a seat per run from a **file mirror** of the retired gateway's `/etc/agi-gateway` | Claude Code (`CLAUDE_CODE_OAUTH_TOKEN`) and Codex CLI (an `auth.json` written into a per-run `CODEX_HOME`) |
| **Caret / Kris** | `secrets.toml` — `providers.claudecode.setup_token`, `providers.codex.api_key` | Claude Code (`CLAUDE_CODE_OAUTH_TOKEN`) and Codex CLI (`CODEX_API_KEY`) |

Two facts that shape everything below:

1. **Nobody uses a virtual key.** All gateway traffic authenticates with one
   shared master key, which carries no service name. Labels do nothing until
   each service holds its own `ck-…`.
2. **The optimizer's mirror is stale.** It reads the *retired* gateway's
   credential directory — 77 accounts where the router serves 139.

And one difference that decides the hard part: **Caret drives the Codex CLI
in API-key mode** (`CODEX_API_KEY`), while **the optimizer drives it in
ChatGPT-subscription mode** (an `auth.json` in `CODEX_HOME`). API-key mode
takes a custom base URL; subscription mode talks to a fixed ChatGPT backend
path.

## 8 · Phase 0 — the question that gates everything

Point each CLI at a development rapid-router with a `ck-…` key and run one
task through it.

| CLI | Expectation | Risk |
|---|---|---|
| Claude Code | `ANTHROPIC_BASE_URL` + token; the router serves `/anthropic/v1/messages` | low |
| Codex, API-key mode | a `model_provider` in `config.toml` with the router's `base_url`; the router serves `/v1/responses` | low |
| Codex, subscription mode | posts to a ChatGPT backend path the router does not expose | **this is the one** |

If subscription mode cannot be repointed, the answer is not to bend the
router — it is that **the optimizer switches its Codex CLI to API-key mode**
and lets the router hold the subscription credentials upstream. The CLI stops
needing to know that ChatGPT seats exist at all. That is what Caret already
does.

Everything below assumes Phase 0 passes in that shape. A few hours' work, and
nothing else should start until it has an answer.

## 9 · Changes by component

### 9.1 Rapid-router

Small, and mostly done.

- **Done:** labels on accounts and keys, enforcement, the two refusals, the
  move API, validation.
- **To do:** the console table from §5; per-service usage in the existing
  usage records so the console can show what each service actually spends.
- **Config:** declare `tenants`, issue three virtual keys, label the 139
  accounts. Do this **after** every consumer holds its key — see §10.
- **Consider:** session stickiness (`x-rapid-session`), so a multi-turn agent
  run stays on one account. Today's least-used-first selection spreads a run
  across accounts, and prompt caching is per account — an agent run with a
  large context would pay full price on every turn. The optimizer's per-run
  seat lease gives it this for free today; routing through the gateway loses
  it unless the gateway offers it.

### 9.2 The optimizer

The biggest change, because it stops owning credentials.

**Repoint the spawn paths.** Both live behind `agipool.SeatLease`:

| Runtime | Today | After |
|---|---|---|
| Claude Code | lease a seat → `CLAUDE_CODE_OAUTH_TOKEN=<seat token>` | `ANTHROPIC_BASE_URL=<router>` + `ANTHROPIC_AUTH_TOKEN=ck-…` |
| Codex | lease a seat → write `auth.json` into the run's `CODEX_HOME` | write a `config.toml` into `CODEX_HOME` naming the router as a `model_provider`, with `CODEX_API_KEY=ck-…` |

Both already write into a per-run `CODEX_HOME`, so the mechanism to place a
file is there; only the file's contents change. Add
`x-rapid-session: <run-id>` per run once the gateway supports it.

**Then delete `internal/app/agipool`** — 13 files. Lease, cooldown, health,
mirror, sanitize, JWT decoding and the local/mirror sources all become the
router's job, and it is already doing them for the traffic it serves.

**And the surrounding wiring:**

- `internal/app/bootstrap/agipool.go` — the boot-time pool check.
- `RAPID_OPTIMIZER_AGENT_ACCOUNTS_MODE` (`local-first` / `local-only`) —
  retired, or reduced to "router" versus "local credentials".
- `scripts/agi-pool-sync.sh` — the mirror sync. Deleted.
- `web/app/optimizer-api/api/v1/{codex,claude}-accounts/*` — the account
  management UI. Either deleted, or repointed at the router's admin API as a
  read-only view. **Do not leave two places that both claim to manage
  accounts.**
- `internal/app/codexmodels/manager.go` — queries the model catalog through a
  `CODEX_HOME`; should read the router's `/v1/models` instead.

**Two bugs this fixes on the way past:** the stale 77-account mirror, and the
credential *collapse* the prod skill warns about (several accounts sharing
one upstream credential because a refreshed `auth.json` landed in the wrong
`CODEX_HOME`). Neither can happen once the optimizer holds no credentials.

### 9.3 Caret / Kris

The smallest of the three, because the hook is already there and unused.

- `internal/platform/config/secrets.go:28` — `ProviderSecret.BaseURL` exists
  and is referenced in exactly two places: its declaration and a redaction
  copy. **It is never used.** Wire it through.
- `internal/adapters/runtime/claudecode/runner.go:227` — add
  `ANTHROPIC_BASE_URL` beside the existing `CLAUDE_CODE_OAUTH_TOKEN`.
- `internal/adapters/runtime/codex/runner.go:141` — the runner already has an
  `ExtraEnv` map, so the base URL and provider settings can go through it
  without restructuring.
- `internal/adapters/runtime/providers/{claudecode,codex,claudecode_purellm,codex_purellm}.go`
  and `registry.go:124` — the same four spawn paths and the env-var map.
- **Configuration:** `base_url` = the router, `api_key` = Kris's `ck-…`.

Caret already uses the Codex CLI in API-key mode, so its Codex path is the
easy one.

## 10 · Order, and the one footgun

1. **Phase 0** — prove both CLIs can be repointed (§8).
2. **Session stickiness** in the gateway, if Phase 0 confirms the caching
   cost is real. Measure a run with and without before relying on it.
3. **Three virtual keys, no labels yet.** Every account stays unassigned, so
   every pool is still shared and nothing changes for anyone.
4. **Caret** — smallest, and proves the path end to end on real traffic.
5. **The optimizer** — repoint, watch, then delete `agipool`.
6. **Label the accounts.** Only now.

> **The footgun:** label an account before all three services hold their own
> key, and everything still on the master key is refused — a key with no
> service owns nothing in a labelled pool. **Keys first, labels last.**

**Rollback at every step:** unassign the accounts (`tenant: null`) and the
pool is shared again; point a consumer's `base_url` back at nothing and it
uses its own credentials again. Nothing is destroyed at any point, because
the credentials never move.

## 11 · Open questions

1. **Codex CLI in subscription mode** — repointable, or must the optimizer
   move to API-key mode? (Phase 0.)
2. **Prompt caching across a multi-turn run** — how much does per-request
   account switching actually cost? Measure before building stickiness.
3. **`/passthrough/…` and stateful endpoints** — the relay now spreads across
   accounts instead of always taking the first. If anything uses it for
   files, batches or fine-tunes, those flows need one account per resource,
   which the change breaks.
4. **Who owns account re-authentication** once the optimizer stops doing it —
   the router has a device-login flow; someone has to watch for dead accounts
   and use it.

## See also

- [virtual-keys.md](virtual-keys.md) — the credentials that carry a service
- [agent-subscriptions.md](agent-subscriptions.md) — where the quota signal
  and the bench window come from
- [router.md](router.md) — health, weights, and the selection this gates
