# Moving Kris onto the shared pool

A plan to review before anything is changed. Nothing here has been done yet.

---

## Part 1 — How Kris works today

**Kris is Caret.** "Kris AI" is the Slack bot's name; `caret` is the program.
One process, `caret serve`, running as a user service on its own box:

| | |
|---|---|
| Host | `i-04e0d535a9c98b95e`, **us-west-2** — a different region from the router |
| Process | `caret serve --config /home/ubuntu/.caret/configs.toml`, listening on `127.0.0.1:8767` |
| Code | `/home/ubuntu/caret-agent`, currently commit `7621fc4b` |
| Repo | `CaretAGI/caret-agent` — a **different GitHub org** from rapidrouter |
| Identity | Slack bot `kris_ai` in the Rapid Claims workspace |

**Kris is Claude-only.** Its `secrets.toml` declares exactly one provider:

```toml
[gateway]
token = …
[providers]
[providers.claudecode]
api_key = …
```

No Codex, no OpenAI. That matters a lot: the awkward part of the optimizer
migration was Codex needing a `config.toml` written into each run's
`CODEX_HOME`, because the CLI ignores `OPENAI_BASE_URL`. **None of that applies
here.** Claude Code honours `ANTHROPIC_BASE_URL` and `ANTHROPIC_AUTH_TOKEN`,
which is the path already proven in production.

### What happens when you message Kris

1. Slack delivers the message to `caret`.
2. `BuildGatewayRuntime` has already booted the runtime at startup.
3. For a conversation, `runner_builder.go` builds a `claudecode.Runner` and
   reads the credential — **setup token first, API key as the fallback**:
   ```go
   authToken := GetProviderSetupToken("claudecode")
   if authToken == "" { authToken = GetProviderAPIKey("claudecode") }
   ```
   On this host only `api_key` is set, so that is what is used.
4. The runner spawns the real `claude` binary with the child's environment
   seeded from `os.Environ()`, then **appends the credential**:
   ```go
   env = append(env, "CLAUDE_CODE_OAUTH_TOKEN="+r.AuthToken)
   ```
5. The CLI talks **straight to Anthropic** as that account. Nothing of ours is
   in the middle — same shape the optimizer had before it moved.

There is a second live spawn path: the **workers manager**, which runs
background tasks and sets the same variable via `providerAuthEnv()`.

---

## Part 2 — What already exists, and why it does nothing

A branch was written on 27 August (`route-through-rapid-router`, one commit).
An independent review found three defects, and I have re-confirmed all three
against the current code.

**1. It never runs.** `applyProviderEndpoints` — the whole feature — is called
from `setupAIProviders`, and `setupAIProviders` has exactly one caller in the
tree: `gateway_bootstrap_test.go`. The real boot path returns without touching
it. Set `base_url` today, restart, and Kris talks to Anthropic exactly as
before, with no error and no log line.

**2. It would send Anthropic's own token to our gateway.** The credential it
presents falls back to the setup token:
```go
func providerKey(...) string {
    if key := GetProviderAPIKey(provider); key != "" { return key }
    return GetProviderSetupToken(provider)   // ← a vendor OAuth token
}
```
A setup token is Anthropic's. It has no business being the bearer on a request
to our gateway.

**3. The vendor credential survives into the run anyway.** The file's own
comment admits it: *"The per-spawn vendor variables are left in place."* Both
live spawn paths append `CLAUDE_CODE_OAUTH_TOKEN` **after** `os.Environ()`, so
whatever the process-level routing set, the child still receives a working
vendor token. Whether the CLI then prefers it over `ANTHROPIC_AUTH_TOKEN` is
not something I want to find out by guessing — the fix is to not send it.

So the honest summary: **the existing branch is a sketch, not a working
change.** I would not rebase and ship it.

---

## Part 3 — What I propose

### 3a · Router side (safe, reversible, nothing breaks)

The `kris` service exists but is empty — **0 accounts, 0 keys**. So:

1. Create a virtual key named `kris` with `tenant = kris`.
2. Move some Codex/Claude accounts to `kris` from `agi`'s 107, in the console.
   How many is your call; Kris is interactive and a human is waiting, so it
   wants a small reserved set rather than a share of the big pool.

Neither step affects anyone: `agi` keeps everything else, and Kris is not
routed yet.

### 3b · Caret side (the real work)

1. **Make it run.** Call the endpoint setup from `BuildGatewayRuntime`, and add
   a test that asserts *boot* reaches it — not one that calls the function
   directly, which is what let this ship broken.
2. **Delete the setup-token fallback.** The gateway credential is the `ck-…`
   key and nothing else. If it is missing, do not route.
3. **Do not hand the child a vendor credential when routed.** Both live spawn
   paths stop appending `CLAUDE_CODE_OAUTH_TOKEN`, the same rule the optimizer
   uses: a routed run carries the virtual key and nothing else.
4. **Drop the Codex entry** from the endpoint table. Kris has no Codex
   provider, and the mapping there is wrong anyway — it names
   `OPENAI_BASE_URL`, which that CLI ignores, and `OPENAI_API_KEY`, while every
   Codex spawn in this repo reads `CODEX_API_KEY`. Better absent than wrong.

### 3c · Configuration

```toml
[providers.claudecode]
base_url = "https://router.rapidclaims.ai/anthropic"
api_key  = "ck-…"        # the kris virtual key, replacing the vendor token
```

Unset `base_url` and everything reverts. That is the rollback, and it is why
the pool credential is left in place rather than deleted.

### 3d · Verification

- One real Slack message to Kris.
- Confirm on the gateway that the request arrived, carried the `kris` key, and
  was served by an account labelled `kris`.
- Confirm from Kris that the reply is normal.

---

## Part 4 — Order, and what could go wrong

| Step | Risk | Undo |
|---|---|---|
| Create the `kris` key | none — additive | delete the key |
| Move accounts to `kris` | none — `agi` keeps the rest | move them back |
| Merge the caret fix | none while `base_url` is unset | revert |
| Set `base_url` + key, restart caret | **Kris is down if wrong** | unset, restart |
| Send a test message | — | — |

The only step with teeth is the fourth, and it is a config edit plus a restart
of a single user service.

**One thing I want to flag:** the restart interrupts any conversation in
flight. Kris is interactive, so unlike the optimizer this is not "nothing is
running at 3am" — it is worth doing when the channel is quiet.

---

## Part 5 — The constraint I cannot solve alone

**The Go caret has no reachable git remote.** This is worse than a missing
permission, and it blocks any code change regardless of what we decide about
accounts.

| Attempt | Result |
|---|---|
| `git ls-remote` on the Kris host → `CaretAGI/caret-agent` | **Permission denied (publickey)** |
| Host-local `gh` (`ashutosh-rapidclaims`, `repo` scope) → same | Could not resolve |
| My local `gh` → same | Could not resolve |
| My local SSH → the Kris host's checkout over `ssh://` | Permission denied |

Searching every org that identity belongs to — `caretagi`, `caret-old`,
`Rapid-Claim`, `opencac`, `rize-sh`, `TensorxSpace` — turns up
**`caretagi/caretagi`**, which is a **Rust** project: `Cargo.toml`, `crates/`,
two commits. The caret running Kris is **Go**: `go.mod` declaring
`module github.com/caretagi/caretagi`, an `internal/` tree, and a history
containing `7621fc4b`, which exists in neither GitHub repo.

So the module path points at a repo holding different code, and the configured
origin points at a repo nobody can open. The host checkout may simply *be* the
source of truth.

Note this is specific to caret. Host-local `gh` works fine for Rapid-Claim
repos — verified against `rapidrouter` and `rapid-optimizer` — via
`scripts/github_ssm_merge_pr.sh`, SSM target `i-04e0d535a9c98b95e`, always with
`export HOME=/home/ubuntu`.

**Someone needs to say where the Go caret source lives** before any of Part 3b
can ship.

---

## Part 6 — What was already tried with Codex, and what it cost

Found on the Kris host: **201 lines of uncommitted work**, dated 29 July, in
`codexapp/runner.go` and `session_router.go`. Not in any commit, branch or
stash, and not in the deployed binary. If that box were rebuilt it would be
gone. It is somebody's attempt to run Kris on Codex, and it is worth reading
before anyone tries again.

The important thing: **the auth and endpoint problems in Part 5 were not what
stopped him.** He got Codex connected and answering. What defeated him was the
*response protocol* — `codex app-server` behaves differently from Claude in
ways Kris's plumbing did not expect.

**1. Kris posted Codex's thinking-out-loud as part of the answer.** Codex tags
agent messages with a `phase`, and `commentary` is internal narration. The
original code concatenated everything into one buffer. His test name says it:
`TestRunner_UsesFinalAnswerWithoutConcatenatingCommentary`.

**2. The Slack message visibly changed after it appeared.** In his words:
*"Streaming it to Slack makes the placeholder briefly show a message that is
later replaced by the final answer."*

**3. Codex often replied with nothing at all.** He added a silent auto-retry
and then a transparent error for when the retry was empty too —
`TestDispatchEvent_ChannelEmptyResponseAutoRetriesThenSendsTransparentError`.

**4. The retry then made it worse, and this is the subtle one.** Claude answers
by calling the `send_message` tool; **Codex answers inline, as plain text**.
Kris read "no `send_message` call" as a missed response and retried an answer
that was already good — *"retrying it wastes a second model turn and can
duplicate side effects."* So turns ran twice, along with whatever they did.

**5. The protocol kept moving.** He had to accept four spellings of one event
— `agentMessage`, `agent_message`, `agent_message_output`, `assistant_message`
— plus *"some app-server versions emit only item/completed… without delta
notifications"* and *"older versions that did not identify message items or
expose their phase."* His text extraction ends in a four-deep fallback chain,
the last rung commented *"defensive compatibility fallback for providers that
mark their only assistant message as commentary."*

**What this means for us.** Routing Kris through the gateway fixes the
credential and endpoint problems and **does not touch any of these** — they
live in Kris's own runner, after a successful connection. Adopting Codex for
Kris therefore means finishing 200 lines of unfinished protocol work before it
is useful. Claude has none of these problems in Kris today.

So: **route Claude for Kris. Leave Codex alone.**

## Part 7 — Why Claude is harder than Codex here, and it is not the mechanism

The obvious question is why the thing that worked for 117 Codex accounts cannot
just be done for Claude. **The mechanism is completely provider-agnostic** —
`owned_by()` never looks at the provider, `managed` is tracked per provider,
and labels work identically on any account.

And Claude routing is not theoretical: **143 requests have already been served
through the gateway on the Claude pool** — `claude-fable-5`, `claude-opus-5`,
`claude-haiku-4-5`, `claude-opus-4-8`, `claude-sonnet-5`, all `200`. It works.

The blocker is **inventory**, and it is sharper than a headcount suggests:

| Account | Models it may serve |
|---|---|
| `ashutoshxhq@gmail.com` | **only** `claude-opus-4-8`, `claude-opus-5` |
| `ashutosh@rapidclaims.ai` | all models |

Two accounts, and one carries a model allowlist. Since a request is matched
against the allowlist *before* ownership is considered, that account cannot
serve `sonnet-5`, `haiku-4-5` or `fable-5` — three of the five models actually
in use.

**So there is effectively one general-purpose Claude account, and you cannot
divide one account between two services.** Give it to `kris` and everything
else loses Sonnet, Haiku and Fable. Give it to `agi` and Kris has nothing.

That is the whole answer: not a design limit, not a code gap — 117 versus 1.

### The way out, in order of preference

1. **Pool the Claude credential Kris already holds.** Kris authenticates today
   with its own `api_key` in `secrets.toml`, which is *not* in the router. Move
   it in and there are two general-purpose accounts — enough to divide.
2. **Add more Claude subscription seats.** The Codex pool works because it has
   117. Claude deserves the same if services are to be isolated on it.
3. **Leave the Claude pool undivided for now.** Give Kris a virtual key with
   `tenant = kris` and label nothing on Claude. Kris then routes through the
   gateway — so its traffic is attributed and visible — while still sharing the
   pool. **This gets most of the value with none of the risk**, and is what I
   would do first.

Option 3 is worth dwelling on: attribution and enforcement are separable. The
key names the service from day one; labelling accounts is a later, independent
decision that can wait for inventory.

### Also worth knowing

`ashutoshxhq@gmail.com` and `ashutosh@rapidclaims.ai` are both personal-looking
accounts, and 12 of the Claude requests came in with **no key at all** — the
open data plane again. Whatever is sending those would break the moment the
Claude pool is divided, for the same reason master-key traffic would have.

## Part 8 — Questions for you

1. **Start with attribution only?** Give Kris a `tenant = kris` key and label
   no Claude accounts (Part 7, option 3). Kris routes, its spend becomes
   visible, nothing can break. Division waits for inventory.
2. **Should Kris's own Claude credential go into the pool?** It is a seat we
   own that the gateway cannot currently see or manage. Pooling it is what
   makes dividing Claude possible at all.
3. **Delivery.** I cannot reach the Go caret repo from anywhere — see Part 5.
   That has to be resolved before any code change can ship, whatever we decide
   about accounts.

---

## Part 9 — Both blockers, closed

Written 2026-08-30, after building and testing both. This supersedes the
"what I cannot solve" framing in Parts 5 and 7 for the two specific
blockers named there. Everything is proved against real binaries; where
something is still unproven it says so.

### 9a · Codex on Kris

**Was:** Kris is Claude-only. It declares no Codex provider, and Part 5's
investigation found the abandoned 29 July attempt plus four reasons it
could not have worked.

**Now:** Kris can run Codex through the gateway.

The mechanism is the one the optimizer proved, arrived at differently.
`OPENAI_BASE_URL` is read by nothing in codex-cli 0.146.0 — it is ignored
silently while the CLI carries on to `api.openai.com`. What redirects it
is a `model_provider` block. The optimizer writes one into each run's
`CODEX_HOME/config.toml`; caret passes the same settings as `-c`
overrides, because:

- caret already drives the CLI that way for its MCP server,
- there is no per-run directory to create, write or clean up, and
- **an override beats the config file**, so whatever sits in the host's
  own `~/.codex/config.toml` cannot quietly redirect a routed run.

`wire_api = "responses"` is not optional. Without it the CLI opens a
transport the gateway does not serve.

**Proved, not asserted.** Two tests drive the real codex-cli against a
recording stand-in, with decoy `OPENAI_API_KEY` and `CODEX_API_KEY`
planted in the environment:

```
real codex-cli reached [/v1/models /v1/models /v1/responses]
  with the virtual key, no contact with OpenAI
caret's Runner reached [/v1/models /v1/models /v1/responses]
  with the virtual key
```

The second matters as much as the first: the override list being right is
worth nothing if caret's Runner does not pass it on.

Two things the stand-in had to get right, learned by watching the real CLI
fail against a first version that got them wrong:

- **It probes the model list before sending anything**, and retries when
  the answer is not a model list. A stand-in that streams at every path
  never gets as far as a completion.
- **The completed event needs an item lifecycle around the text**, or the
  client has nothing to attach the deltas to.

### 9b · Claude accounts, poolable like Codex ones

**Was:** Part 7 said the blocker was inventory — 117 Codex accounts versus
effectively one general-purpose Claude account — and that the mechanism
was fine. That was half right, and the wrong half was load-bearing.

The mechanism *for ownership* was fine: `owned_by()` never looks at the
provider, and `claude_subscription` accounts take a `tenant` like any
other. But there was a second gap underneath, in `refresh.rs`:

```rust
// Codex is the only transport whose refresh flow is implemented.
if kind != ProviderKind::CodexSubscription { return false; }
```

**Nothing renewed a Claude seat's token.** A Claude access token lives
hours, not Codex's ten days. So a Claude subscription seat added to the
pool would work until it expired that afternoon and then die, needing a
human with a browser. That is why the gateway carries **119 Codex seats
and zero Claude ones**, and why the two Claude accounts there are API
keys rather than seats.

> **A correction, and a small finding.** An earlier draft of this said
> "139 credential files", counting files on disk rather than accounts.
> There are 139 files but only **119 distinct accounts**: 15 accounts
> were uploaded twice under two naming schemes (`codex_aisha-kapoor.json`
> and `codex_aisha-kapoor-opencac-com.json` are one account), leaving 20
> orphans.
>
> They are not wired into the config — for all 15 pairs exactly one file
> is a live key, and the orphans' mtimes are frozen at their 17 August
> upload while every live file has rotated since. So routing is
> unaffected. But they hold real OAuth refresh tokens that nothing
> renews, so they are dead credentials sitting on disk, and a future bulk
> re-import could pick them up. Two entries for one account is a known
> hazard: each seat carries its own breaker, so traffic keeps hitting an
> account whose twin was just benched for being out of quota. Worth
> deleting the 20.

Inventory was a symptom. You cannot accumulate seats you cannot keep alive.

**Now:** Claude seats renew themselves, so a Claude subscription login is
poolable exactly like a Codex one. Any machine with a Claude subscription
can sign in and hand over its `~/.claude/.credentials.json`.

The values were read out of the Claude Code CLI's own bundle (v2.1.238),
**not guessed** — the distinction matters more here than anywhere else in
the gateway, because a refresh token rotates the instant the endpoint
answers, so a wrong URL or client id does not merely fail, it spends the
credential it was trying to preserve.

| | Codex | Claude |
|---|---|---|
| Token URL | `auth.openai.com/oauth/token` | `platform.claude.com/v1/oauth/token` |
| Client id | `app_EMoamEEZ73f0CkXaXp7hrann` | `9d1c250a-e61b-44d9-88ed-5944d1962f5e` |
| Body | form-urlencoded | **JSON** |
| Expiry | the token's own JWT `exp` | `now + expires_in × 1000` |
| Document | `tokens.*`, snake_case | `claudeAiOauth.*`, camelCase |

The expiry difference is the one that bites: the response says "3600
seconds from now" and the document wants an absolute instant in
milliseconds. Write the response through unchanged and the seat is treated
as expired on every subsequent request — no error, just a pool that never
serves.

Scopes are sent back from the seat's own document rather than a default
list, so a renewal cannot silently widen or narrow the grant.

**Proved end to end.** The e2e harness now carries an expired Claude seat
backed by a credential file, and the gateway's refresh is pointed at the
stand-in (`RAPID_CLAUDE_OAUTH_URL` — a real refresh can never be tested
against the vendor, because the first attempt would be the only one that
seat ever gets). It asserts the renewal happened, in the right encoding,
with the right client id, presenting the stale token and the seat's own
scopes; that the vendor then saw the *renewed* token; and that the rotated
refresh token and an absolute `expiresAt` were persisted to disk — because
if the rotated token is not recorded, the seat can never renew again.

The console now accepts Claude credential files the same way it accepts
Codex ones, in bulk. One honest limitation: **Claude's document carries no
email**, so those seats are named after their files. Name the files after
the accounts before uploading forty of them.

### 9c · What this does and does not change

**Does:** Claude seats can now be accumulated, labelled and divided
between services on exactly the same terms as Codex seats. The 117-vs-1
asymmetry is now a matter of going and collecting logins, not a gap in the
gateway.

**Does not:** it adds no accounts by itself. Today's Claude pool is still
two API keys, one of them carrying a model allowlist. Part 7's
recommendation stands unchanged for right now — give Kris a `tenant = kris`
key and label nothing on Claude, so its spend becomes visible without
dividing a pool that cannot yet be divided. Division becomes worth doing
once there are seats to divide.

**Still open:** the Go caret has no reachable git remote (Part 5). Both
commits live on my machine and, once deployed, on the host — the same
position as the 29 July work. That is tolerable for a week and bad for a
quarter.
