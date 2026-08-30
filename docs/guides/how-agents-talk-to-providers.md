# How Kris talks to Claude and ChatGPT

Written from scratch, assuming no prior knowledge. Every header and body in
here was captured from a real running system, not invented.

---

## Part 1 — The one idea everything else depends on

There are **three completely different ways** to talk to an AI provider. They
are not variations on a theme; they use different URLs, different credentials,
and different billing. Almost every confusing thing in this system comes from
mixing them up.

### Way 1 — An API key (metered)

You buy a key from the vendor's developer console. It looks like
`sk-ant-api03-…` or `sk-proj-…`. You send it on every request and you are
billed **per token**.

```http
POST https://api.anthropic.com/v1/messages
x-api-key: sk-ant-api03-xxxxx
content-type: application/json
```

Simple, works from anywhere, costs money per word.

### Way 2 — A subscription seat (flat rate)

This is a **Claude Pro/Max** or **ChatGPT Plus/Pro** login — the same account a
human signs into. You pay a monthly fee and get a usage allowance.

You do not get an API key. You get an **OAuth token**, obtained by logging in
through a browser. It expires in hours and is refreshed automatically. For
Codex it lives in a file called `auth.json`; for Claude Code it is a token
string.

```http
POST https://api.anthropic.com/v1/messages
Authorization: Bearer sk-ant-oat01-xxxxx      ← "oat" = OAuth token
```

**This is what we own 119 of.** They are flat-rate, which is exactly why they
are worth pooling and worth protecting.

### Way 3 — The vendor's own private backend

The vendor's CLI does not always use the public API. `codex` in subscription
mode talks to a **private endpoint** meant only for that CLI:

```
https://chatgpt.com/backend-api/codex/responses
```

It sends headers that identify it as the real CLI, and it refuses to work if
they are wrong. This is the mode that caused all the trouble, and Part 5 is
about it.

> **The single most useful sentence in this document:** an API key and a
> subscription are not interchangeable. A tool built for one will not work with
> the other, no matter how the environment variables are set.

---

## Part 2 — What Kris does today, step by step

Kris is a Slack bot. The program is called `caret`. Here is a full round trip.

**1. You type in Slack.** `caret` receives it.

**2. Caret picks a credential.** From `runner_builder.go`:

```go
authToken := GetProviderSetupToken("claudecode")          // first choice
if authToken == "" {
    authToken = GetProviderAPIKey("claudecode")           // fallback
}
```

On the Kris host only `api_key` is filled in, so that is what gets used.

**3. Caret starts the real `claude` program** as a child process, and hands it
the credential through an environment variable:

```go
env := os.Environ()
env = append(env, "CLAUDE_CODE_OAUTH_TOKEN="+r.AuthToken)
cmd.Env = env
```

An **environment variable** is just a named value a program can read when it
starts. It is how one program passes settings to another.

**4. The `claude` CLI calls Anthropic directly.** Caret is no longer involved:

```http
POST https://api.anthropic.com/v1/messages?beta=true
Authorization: Bearer sk-ant-oat01-…
content-type: application/json
anthropic-version: 2023-06-01

{
  "model": "claude-sonnet-4-5",
  "max_tokens": 8192,
  "messages": [
    { "role": "user", "content": "summarise this thread" }
  ],
  "stream": true
}
```

**5. Anthropic streams the answer back**, the CLI prints it, caret reads it and
posts to Slack.

### The important part

At step 4, **Kris is holding the actual subscription credential and spending it
directly.** Nothing of ours sits in the middle. Which means:

- We cannot see what Kris spent.
- We cannot stop Kris from spending an account another service needs.
- Every place that holds a credential is a place it can leak from.

That is the whole reason for the change.

---

## Part 3 — What routing changes

Only two things change: **where** the CLI sends the request, and **what
credential** it presents.

### Before

```http
POST https://api.anthropic.com/v1/messages        ← the vendor
Authorization: Bearer sk-ant-oat01-…              ← our subscription seat
```

### After

```http
POST https://router.rapidclaims.ai/anthropic/v1/messages   ← our gateway
Authorization: Bearer ck-0a1b2c-…                          ← a virtual key
```

Set with two environment variables:

```bash
ANTHROPIC_BASE_URL=https://router.rapidclaims.ai/anthropic
ANTHROPIC_AUTH_TOKEN=ck-0a1b2c-…
```

**The body does not change at all.** Same JSON, same model, same messages. The
gateway speaks Anthropic's dialect on purpose so nothing upstream of it needs
to know it exists.

### What is a `ck-…` key?

A **virtual key** we issue. Shape: `ck-<id>-<secret>`, e.g.
`ck-0a1b2c-optimizersecret0123`. It is not a vendor credential and is worthless
to Anthropic. It only means something to our gateway, which looks it up and
finds:

- which **service** this is (`kris`, `agi`, `optimizer`)
- which accounts that service is allowed to spend
- any rate limit or budget on it

### Then the gateway does what Kris used to do

```http
POST https://api.anthropic.com/v1/messages
Authorization: Bearer sk-ant-oat01-…      ← the real seat, chosen by the gateway
```

The subscription credential never leaves the gateway. Kris never sees one.

**This is verified**, not theoretical. Against a real gateway, `claude` sent:

```
POST /v1/messages?beta=true
Authorization: Bearer ck-ROUTED-VIRTUAL-KEY
```

...even with a decoy vendor credential planted on disk. The virtual key won.

---

## Part 4 — Exactly what the gateway sends to OpenAI

You asked for the precise headers. This is a real capture from a live run —
the gateway's outbound request for a Codex subscription seat, recorded by a
listener standing in for `chatgpt.com`:

```http
POST /backend-api/codex/responses HTTP/1.1
authorization:      Bearer eyJhbGciOiJSUzI1NiJ9.eyJleHAiOjQwMDAwMDAwMDAsImFjY3QiOiJhY2N0LW9wdCJ9.…
chatgpt-account-id: acct-opt
originator:         codex_cli_rs
user-agent:         codex_cli_rs/0.146.0
version:            0.146.0
openai-beta:        responses=experimental
session_id:         01a0495a-bee6-7b22-8dac-6a15c9a8ba67
accept:             text/event-stream
accept-encoding:    identity
content-type:       application/json
```

```json
{
  "model": "gpt-5.6-sol",
  "instructions": "You are a coding agent…",
  "input": [ { "role": "user", "content": [ … ] } ],
  "reasoning": { "effort": "medium" },
  "text": { "verbosity": "medium" },
  "store": false,
  "stream": true,
  "prompt_cache_key": "01a0495b-17f2-…"
}
```

Reading it line by line:

| Header | What it is for |
|---|---|
| `authorization: Bearer eyJ…` | The seat's OAuth access token. A **JWT** — three base64 chunks separated by dots. The middle chunk decodes to `{"exp":4000000000,"acct":"acct-opt"}`, which is how I proved the *right* account served the request. |
| `chatgpt-account-id` | Which ChatGPT account this is. Must match the token. |
| `originator` / `user-agent` / `version` | The backend is private to the CLI, so the gateway **identifies itself as the CLI**. Get these wrong and the request is refused. |
| `openai-beta: responses=experimental` | Opts into the Responses API shape. |
| `accept: text/event-stream` | "Stream it back to me token by token" (SSE — Server-Sent Events). |

And the body:

| Field | Meaning |
|---|---|
| `instructions` | The system prompt — the standing brief. |
| `input` | The conversation so far. |
| `reasoning.effort` | How hard to think. Costs more, takes longer. |
| `store: false` | Do not keep this server-side. |
| `prompt_cache_key` | Lets the vendor reuse work from the previous turn **on the same account**. Cheaper and faster. Remember this one — it matters in Part 6. |

For comparison, the **Anthropic** shape is much plainer — no impersonation
headers, because it is a public API:

```http
POST /v1/messages
Authorization: Bearer sk-ant-oat01-…
anthropic-version: 2023-06-01
```
```json
{ "model": "claude-sonnet-4-5", "max_tokens": 1024,
  "messages": [ { "role": "user", "content": "hi" } ] }
```

---

## Part 5 — Why your ChatGPT attempt did not work

You are not imagining it. There are **four** separate reasons, and any one of
them alone would have sunk it.

### Reason 1 — Caret's Codex path wants an API key, not your subscription

Every Codex spawn in caret does this:

```go
env = append(env, "CODEX_API_KEY="+p.authToken)
```

`CODEX_API_KEY` means **Way 1** — a metered developer API key you buy
separately. Putting a ChatGPT subscription login there does nothing: it is the
wrong kind of credential in the wrong variable. Your ChatGPT Plus subscription
cannot be typed into that field in any form.

### Reason 2 — `OPENAI_BASE_URL` is ignored

The obvious way to point the CLI elsewhere. **codex-cli 0.146.0 ignores it
completely** — no warning, no error. It carries on to `api.openai.com` while
looking perfectly configured.

We found this the expensive way: a run that appeared routed spent a real
account. The only thing that actually redirects it is a block in the run's
`CODEX_HOME/config.toml`:

```toml
model_provider = "rapidrouter"

[model_providers.rapidrouter]
base_url = "http://gateway/v1"
env_key  = "RAPID_ROUTER_KEY"
wire_api = "responses"      # ← without this it opens a transport we don't serve
```

### Reason 3 — subscription mode cannot be pointed anywhere

If you use the CLI the normal way — `codex login`, ChatGPT subscription — it
talks to that private `chatgpt.com` backend. You can redirect the model call
with `chatgpt_base_url`, but the CLI then **refreshes its token against a
hard-coded auth host** and rejects any `auth.json` it did not create itself.

So: subscription mode works, but only against OpenAI. It cannot be pooled by us
at the CLI end. The way out is to invert it — put the CLI in API-key mode
pointing at *our* gateway, and let the **gateway** hold the subscriptions. The
CLI stops needing to know they exist.

### Reason 4 — even the routing code had the variable name wrong

The routing sketch in caret maps Codex to `OPENAI_API_KEY`:

```go
{provider: "codex", baseURL: "OPENAI_BASE_URL", token: "OPENAI_API_KEY"},
```

...while every Codex spawn in that same repo reads `CODEX_API_KEY`. So the key
would have been placed in a variable nothing reads. Two wrong names on one
line.

### So: is ChatGPT usable for Kris?

Yes — but through the gateway, not directly. The gateway already serves 119
Codex subscription seats and it works: **verified with a real optimizer issue**
end to end. Kris would point at the gateway the same way.

But right now Kris has no Codex provider configured at all, and no reason to.
Claude is the simpler and better-supported path, and it is the one Kris uses.

---

## Part 6 — Things that will confuse you later

**Prompt caching is per account.** That `prompt_cache_key` only helps if the
*same* account serves the next turn. A gateway that spreads requests across
accounts can lose the cache and pay full price every turn. Unmeasured so far,
and the main open cost question.

**"Codex" means three things.** The CLI, the model family, and our provider
name. Usually clear from context, occasionally not.

**A JWT is readable.** The `eyJ…` token is base64, not encryption. Anyone
holding it can read the account inside — which is how I verified the right seat
served. It is still a secret: readable is not the same as harmless.

**Streaming changes the shape.** With `stream: true` the reply is not one JSON
document but a sequence of `event:`/`data:` lines. This bit us: our test double
omitted an `id` field on the final event and sent text without the
`output_item.added` / `.done` wrapper around it. The real CLI refused the first
and silently showed an empty answer for the second.

---

## Glossary

| Term | Plain meaning |
|---|---|
| **API key** | A password you buy. Billed per word. |
| **OAuth token** | A temporary pass from logging in. Expires in hours, auto-renews. |
| **Subscription seat** | A monthly-fee account. What we own 119 of. |
| **Virtual key** (`ck-…`) | Our own key. Names a service; worthless to a vendor. |
| **Environment variable** | A named value a program reads at startup. |
| **JWT** | A token in three base64 parts. The middle part is readable data. |
| **SSE** | Streaming over HTTP — the reply arrives in pieces. |
| **`CODEX_HOME`** | The folder the Codex CLI keeps its login and config in. |
| **Dialect** | Which API shape a request is in — Anthropic's, OpenAI's, Responses. |
| **Provider** | A pool of accounts in the gateway: `claude`, `codex`, `openai`. |
| **Service / tenant** | Who is asking: `agi`, `kris`, `optimizer`. |
