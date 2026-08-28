# Completed — service-owned accounts

Branch `service-accounts`, worktree `rapidrouter-service-accounts`, based on
`9af5cc0`.

**Every gate passes.** Numbers below are from actual runs, not claims — the
commands are recorded so you can repeat them.

---

## 1 · What was built

**The rule:** an account carries the name of the service it belongs to, a
key carries the name of the service it is, and a request may spend the
accounts whose label matches — and no others.

### Configuration

```toml
tenants = ["kris", "agi", "optimizer"]   # top-level, before any [table]

[providers.codex]
keys = [
  { name = "seat-01", value = "file:…", tenant = "kris" },
  { name = "seat-02", value = "file:…" },                 # unassigned
]

[[virtual_keys]]
name   = "optimizer-runner"
tenant = "optimizer"
```

### Enforcement — `ProviderRuntime::owned_by`, 5 lines

```rust
if !self.managed { return true; }        // no labels here: shared, as before
match (key.tenant.as_deref(), tenant) {
    (Some(owner), Some(caller)) => owner == caller,
    _ => false,                          // unassigned account, or key with no service
}
```

Applied at credential selection, so every path goes through it — routed
requests, the relay, and the media path.

### Moving an account

```
PUT /admin/api/providers/{provider}/keys/{account}/tenant  {"tenant": "kris"}
PUT …                                                      {"tenant": null}   # unassign
```

One field on one account. The source is implied.

### Refusals

| Situation | Answer |
|---|---|
| Owns no account here | `403` — *service `x` owns no account on provider `p` that can serve model `m`* |
| Owns accounts, all spent | `429` — *service `x` has no account left on provider `p`: all N of its accounts are out of quota* |

### Validation, at config load

- An account labelled with an undeclared service.
- A key labelled with an undeclared service.
- A duplicate name in `tenants`.

---

## 2 · Why it ended up this shape

Three earlier designs were built and deleted on the way here. Recorded
because the reasons still apply if anyone is tempted to re-add them:

| Deleted | Why |
|---|---|
| A per-key hash slice (`max_accounts`) | Invisible, unstable and unmovable — you could not say *which* accounts, and the assignment shifted under you. Measured: it also left ~40% of a 70-account pool idle while oversubscribing the rest. |
| Floors + priority + cutoffs | Protected *access* at the end of the pool but never limited *consumption*, which was the actual worry. And "AGI gets the rest" made the lowest-priority service permanently paused. |
| Borrowing between services, ownership leases, a reconciler, hysteresis | All artifacts of partitioning the pool and then needing to un-partition it. Deleting the partition deleted the problem. **Not to be confused with `/v1/accounts/lease`** (§3c), which is a different thing entirely: lending a credential to a client that cannot be proxied, not moving ownership between services. |
| A utilization gate on lending | Measured wrong: the router normalizes load, so a pool crosses any threshold together — the gate was open when nobody needed it and shut when they did. |
| An `accounts = […]` pin on keys | A second mechanism answering the same question as the label. Two overlapping answers is how the relay bypass happened. |

---

## 3 · What was tested, and how

No Rust toolchain exists on this Mac. Everything below ran in a container:

```bash
docker run --rm -m 8g \
  -v "$PWD":/work -w /work \
  -v rr-cargo-registry:/usr/local/cargo/registry \
  -v rr-target:/target -e CARGO_TARGET_DIR=/target \
  -e CARGO_BUILD_JOBS=1 rust:1 <command>
```

`rust:1` resolved to **1.98.0-aarch64-unknown-linux-gnu**.

### Gate results

| Gate | Command | Result |
|---|---|---|
| G1 formatting | `cargo fmt --all --check` | **PASS** |
| G2 compiles | `cargo check --workspace --all-targets` | **PASS**, no errors |
| G3 lints | `cargo clippy --workspace --all-targets` | **PASS**, 0 warnings |
| G4 tests | `cargo test --workspace` | **440 passed, 0 failed** |
| G5 console types | `npm run typecheck` | **PASS** |
| G6 console build | `npm run build` | **PASS** (234 kB js, 46 kB css) |

G1–G4 are the same four commands `.github/workflows/ci.yml` runs, so this is
CI-equivalent.

### New tests written for this change — 28

**Unit, `router-core/src/router.rs`** (6)

| Test | Proves |
|---|---|
| `a_service_spends_only_the_accounts_labelled_for_it` | a service reaches its own accounts and no others |
| `an_unassigned_account_serves_nobody` | an unlabelled account is not a free-for-all |
| `a_caller_with_no_service_gets_nothing_from_a_labelled_pool` | a key with no service owns nothing |
| `an_unlabelled_pool_is_shared_by_everyone` | **the backwards-compatibility guarantee** |
| `one_service_running_dry_does_not_touch_another` | no fall-through; and it is reported as out-of-quota |
| `holdings_are_what_the_console_reads` | owned/usable counts are right |

**Config validation, `router-core/tests/config_validation.rs`** (5)

`a_key_names_the_service_it_belongs_to`,
`a_key_may_not_name_a_service_nobody_declared`,
`an_account_may_not_name_a_service_nobody_declared`,
`duplicate_service_names_are_rejected`,
`account_labels_resolve_onto_the_provider`.

**End to end, `router-server/tests/e2e_vkey_accounts.rs`** (7) — each reads
the credential the mock upstream actually received, so "it worked" means
traffic went where it should, not that a status code was 200.

| Test | Proves |
|---|---|
| `a_service_spends_only_its_own_accounts` | 30 requests, only agi's two credentials ever reach upstream |
| `two_services_never_share_an_account` | the two services' credential sets are disjoint |
| `a_key_with_no_service_owns_nothing` | 403, and **nothing reached upstream** |
| `an_unlabelled_pool_serves_everyone` | an undivided pool is untouched by any of this |
| `a_service_out_of_quota_does_not_reach_another_service` | a benched subscription seat gives 429 naming the count, and its service does **not** fall through to another's seat |
| `the_relay_honours_the_labels` | `/passthrough/…` stays inside the caller's accounts — the bypass this work found |
| `a_service_does_not_weaken_authentication` | a bad secret is still 401 |

**End to end, `router-server/tests/e2e_account_moves.rs`** (7) — the
management operations, against a store-backed gateway.

| Test | Proves |
|---|---|
| `giving_an_account_to_a_service_moves_its_traffic` | 403 before, 200 after, and traffic lands on exactly the account given |
| `unassigning_an_account_takes_it_out_of_service` | `null` puts it back to belonging to nobody |
| `moving_an_account_to_a_service_that_does_not_exist_is_refused` | 422, and the assignment is unchanged |
| `moving_an_account_that_does_not_exist_is_a_404` | not a silent no-op |
| `an_account_can_be_added_straight_into_a_service` | a new account can arrive already owned, and serves immediately |
| `adding_an_account_for_a_ghost_service_is_refused` | 422 rather than an account that serves nobody |
| `deleting_an_account_takes_it_out_of_its_service` | its service is told it owns nothing, and does not fall through |

### Regressions

Every pre-existing suite still passes, including `e2e_conformance`,
`e2e_dialect_matrix`, `e2e_reliability`, `e2e_responses`,
`e2e_subscriptions`, `e2e_passthrough`, `selection_properties`,
`loom_models`, `fleet`, `backends`, `store`. That is the 432 total.

### Bugs found by these tests

Four, all mine, all fixed:

1. A router test fixture named a provider `codex` without a type — the
   validator rightly refuses an unknown provider name.
2. `tenants = [...]` placed *after* a `[table]` header in two test configs —
   TOML then reads it as a field of that table. Top-level keys must come
   first.
3. The same ordering bug in the e2e fixture.
4. `run_passthrough` crossed clippy's argument limit once `vk` was threaded
   through; annotated with a reason, matching three existing precedents in
   the same file.

---

## 3b · The management UI

Both surfaces are built.

**Providers drawer** — each credential row now has a **Service** column with
a dropdown of the declared services (plus *Unassigned*). Changing it moves
the account. The add-credential form offers the same dropdown, so an account
can arrive already owned rather than spending a moment belonging to nobody.

**Virtual keys page** — each key shows its **Service** (a dropdown that
reassigns the key) and an **Accounts** button with the count its service
owns. That opens a drawer listing those accounts across every pool, with:

- **Remove** on each — unassigns it. The account is not deleted, and the
  drawer says so.
- **Add an account** — a picker of every account not already owned by this
  service, each labelled with its current owner, so taking one from another
  service is a visible act rather than an accident.

The drawer's subtitle states the thing that is easy to get wrong: accounts
belong to the *service*, not to the key you opened it from, so every key of
that service is affected.

Server side, the providers endpoint now reports each account's `tenant` and
the pool's `managed` flag, plus the roster of declared services — so the
console offers names rather than making an operator type one that has to
match exactly.

**Not verified by an automated test:** the console changes typecheck and
build, and every API call behind them has an end-to-end test, but no
browser test drives the new controls. The existing Playwright suite was not
extended.

## 3c · Lending a credential (`POST /v1/accounts/lease`)

Added after live testing showed a subscription-mode Codex CLI cannot be
pointed at the gateway. A service asks with its virtual key, is handed an
account **labelled for it**, and drives its own CLI with it. Same ownership
rule; `403` if it owns none, `429` if all of its own are spent.

Two guards: `lease_accounts` must be set on the key (holding a credential is
more than spending it through us), and the refresh token is blanked on the
way out (the gateway must be the only writer that rotates a credential).

Five e2e tests in `e2e_account_lease.rs`, including that a live refresh token
never appears in what goes out. Three unit tests on the defanging helper.

**The trade:** traffic on a lent account goes straight to the vendor, so the
gateway sees the lease and not the requests.

## 4 · What is not done

| Item | Why it matters |
|---|---|
| **Per-service usage metrics** | You cannot yet see what each service spends, which is what would tell you a service needs another account. |
| **Session stickiness** (`x-rapid-session`) | A multi-turn agent run is spread across accounts, and prompt caching is per account — so an agent run may pay full price every turn. Unmeasured. Blocks the optimizer migration. |
| **Optimizer and Caret migration** | Neither has been touched. Plan in [docs/components/account-pools.md](docs/components/account-pools.md) §7–§10. |
| **Codex CLI in subscription mode** | Unknown whether it can be pointed at the router. Gates the optimizer work. |

### Known risk, not a bug

`/passthrough/…` now spreads across a service's accounts instead of always
using the first. If anything relays *stateful* endpoints — files, batches,
fine-tunes — those need the same account each time, and this change breaks
them. Nothing in the repo does; the consumers were not audited.

---

## 5 · Note on the working tree

This branch was lifted out of the main checkout while somebody else's
`trace_keys` / `ttft_ms` / `HistoryFilter.meta` work sat uncommitted in the
same files, so it came across carrying part of that feature.

**Removed on 2026-08-28, when the branch was rebased onto `origin/main`.**
That work had by then been merged as #24 and reverted as #25, so carrying it
here would have re-landed something main had deliberately backed out — under
the wrong author. What was taken out: the `trace_keys` / `trace_value_chars`
config fields and their validation, `DEFAULT_TRACE_KEYS`,
`canonical_trace_key`, four tests in `config_validation.rs`, the
`meta` / `MetaFilter` / `appendMeta` / `error_class` / `seat` / `ttft_ms` /
`queue_lag_ms` surface in `console/src/api.ts`, and the meta filter, facets
and detail-drawer rows in `console/src/app.tsx` — for which `Requests`,
`RequestDrawer` and `RequestRows` were restored wholesale from `origin/main`,
as was `console/src/styles.css`.

Verified afterwards: `git diff origin/main` adds no line mentioning any of
those identifiers, and `tsc --noEmit` and the Rust suite are both clean.

## 6 · Repeating the verification

```bash
cd rapidrouter-service-accounts
docker run --rm -m 8g -v "$PWD":/work -w /work \
  -v rr-cargo-registry:/usr/local/cargo/registry \
  -v rr-target:/target -e CARGO_TARGET_DIR=/target -e CARGO_BUILD_JOBS=2 \
  rust:1 sh -c "rustup component add rustfmt clippy && \
    cargo fmt --all --check && \
    cargo clippy --workspace --all-targets && \
    cargo test --workspace"

cd console && npm run typecheck && npm run build
```

Note `-m 8g` and `CARGO_BUILD_JOBS=2`: linking the test binaries in parallel
exhausted the container's default memory and failed with a bare
`linking with cc failed`, which looks like a code error and is not one.
