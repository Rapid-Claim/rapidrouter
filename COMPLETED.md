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
| Borrowing, leases, a reconciler, hysteresis | All artifacts of partitioning the pool and then needing to un-partition it. Deleting the partition deleted the problem. |
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
  -e CARGO_BUILD_JOBS=2 rust:1 <command>
```

`rust:1` resolved to **1.98.0-aarch64-unknown-linux-gnu**.

### Gate results

| Gate | Command | Result |
|---|---|---|
| G1 formatting | `cargo fmt --all --check` | **PASS** |
| G2 compiles | `cargo check --workspace --all-targets` | **PASS**, no errors |
| G3 lints | `cargo clippy --workspace --all-targets` | **PASS**, 0 warnings |
| G4 tests | `cargo test --workspace` | **429 passed, 0 failed** |
| G5 console types | `npm run typecheck` | **PASS** |
| G6 console build | `npm run build` | **PASS** (234 kB js, 46 kB css) |

G1–G4 are the same four commands `.github/workflows/ci.yml` runs, so this is
CI-equivalent.

### New tests written for this change — 17

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

**End to end, `router-server/tests/e2e_account_moves.rs`** (4) — the
management operation, against a store-backed gateway.

| Test | Proves |
|---|---|
| `giving_an_account_to_a_service_moves_its_traffic` | 403 before, 200 after, and traffic lands on exactly the account given |
| `unassigning_an_account_takes_it_out_of_service` | `null` puts it back to belonging to nobody |
| `moving_an_account_to_a_service_that_does_not_exist_is_refused` | 422, and the assignment is unchanged |
| `moving_an_account_that_does_not_exist_is_a_404` | not a silent no-op |

### Regressions

Every pre-existing suite still passes, including `e2e_conformance`,
`e2e_dialect_matrix`, `e2e_reliability`, `e2e_responses`,
`e2e_subscriptions`, `e2e_passthrough`, `selection_properties`,
`loom_models`, `fleet`, `backends`, `store`. That is the 429 total.

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

## 4 · What is not done

| Item | Why it matters |
|---|---|
| **Console table** with per-service rows and the +/− control | The API exists and is tested; there is no UI. Today a move is a `curl`. `holding()` already returns the two counts each row needs. |
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

This branch was lifted out of the main checkout, which had other people's
uncommitted work interleaved in the same files. `proxy.rs` and `admin.rs`
came across carrying half of somebody's `trace_keys` / `ttft_ms` /
`HistoryFilter.meta` feature and did not compile. Both were reset to `HEAD`
and this change re-applied cleanly on top.

Three files still carry ~31 lines of that work, because it is interleaved
with mine and cannot be separated by hand: `config/raw.rs`, `config/mod.rs`,
`config/validate.rs`. It is additive (new config fields), it compiles, and
git will see it as already-present when this rebases onto a main that has it
committed. `console/src/app.tsx` may likewise carry some of their console
work; it typechecks and builds.

**The main checkout was left untouched.** Nothing there was reverted,
because whose work is whose could not be established with confidence.

---

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
