# Implementation plan — service-owned accounts

Branch `service-accounts`, worktree `rapidrouter-service-accounts`.

**The rule being implemented:** an account carries the name of the service
it belongs to, a key carries the name of the service it is, and a request
may spend the accounts whose label matches — and no others.

Design: [docs/components/account-pools.md](docs/components/account-pools.md).

---

## 0 · How this gets tested

There is **no Rust toolchain on this Mac** (no `cargo`, no `rustc`, no
`rustup`). Docker is running, so everything is compiled and tested inside a
`rust:1` container with the worktree mounted:

```bash
docker run --rm \
  -v "$PWD":/work -w /work \
  -v rr-cargo-registry:/usr/local/cargo/registry \
  rust:1 <command>
```

The named volume keeps the crates.io registry between runs so only the first
build is slow.

**Nothing in this plan counts as done until its test has actually run and
passed in that container.** A test that is written but not executed is
recorded as "written, not run" and nothing else.

The console is TypeScript and builds locally (`node` is present), so its
check is `npm run typecheck` in `console/`.

### The gates, in order

| Gate | Command | Must be |
|---|---|---|
| G1 formatting | `cargo fmt --all --check` | clean |
| G2 compiles | `cargo check --workspace --all-targets` | no errors |
| G3 lints | `cargo clippy --workspace --all-targets` | no warnings (CI runs `-D warnings`) |
| G4 unit + integration | `cargo test --workspace` | all pass |
| G5 console types | `npm run typecheck` in `console/` | clean |
| G6 console build | `npm run build` in `console/` | succeeds |

G1–G4 mirror `.github/workflows/ci.yml` exactly, so passing here means
passing CI.

---

## 1 · Scope

### In scope

1. `tenants = [...]` — the list of valid service names.
2. `tenant` on a provider account — who owns it.
3. `tenant` on a virtual key — who the caller is.
4. Enforcement at credential selection, on every path including the relay.
5. Two distinct refusals: owns-nothing (403) and all-mine-spent (429).
6. `PUT /admin/api/providers/{provider}/keys/{account}/tenant` — the move.
7. `holding()` — accounts owned and usable per service, for the console.
8. Validation that rejects a bad config at load.

### Out of scope for this branch

- The console table with the +/− controls (API exists; UI does not).
- Per-service usage metrics.
- Session stickiness (`x-rapid-session`).
- Any change to the optimizer or Caret.

---

## 2 · Work items, each with its test

### W1 — Config surface

**Change.** `RawConfig.tenants: Vec<String>`; `RawKey.tenant: Option<String>`;
`RawVirtualKey.tenant: Option<String>`. Resolved into `Config.tenants:
BTreeSet<String>` and `ApiKey.tenant`.

**Tests** (`crates/router-core/tests/config_validation.rs`):

- `a_key_names_the_service_it_belongs_to` — a valid config resolves the key's
  service and the registry.
- `account_labels_resolve_onto_the_provider` — a labelled account carries its
  service; an unlabelled one is `None`.

### W2 — Validation

**Change.** Reject: an account labelled with an undeclared service; a key
labelled with an undeclared service; a duplicate name in `tenants`.

**Tests** (same file):

- `a_key_may_not_name_a_service_nobody_declared`
- `an_account_may_not_name_a_service_nobody_declared`
- `duplicate_service_names_are_rejected`

Each asserts the exact error path and a fragment of the message, so a
reworded error cannot silently stop being helpful.

### W3 — Enforcement

**Change.** `KeyRuntime.tenant`; `ProviderRuntime.managed` (true when any
account in the pool carries a label); `owned_by()`; the filter in
`admit_for`; `holding()`; `healthy_key_count` and `all_keys_benched` scoped
to the caller.

**Tests** (`crates/router-core/src/router.rs`):

- `a_service_spends_only_the_accounts_labelled_for_it`
- `an_unassigned_account_serves_nobody`
- `a_caller_with_no_service_gets_nothing_from_a_labelled_pool`
- `an_unlabelled_pool_is_shared_by_everyone` — the backwards-compatibility
  guarantee.
- `one_service_running_dry_does_not_touch_another`
- `holdings_are_what_the_console_reads`

### W4 — Refusals

**Change.** `no_accounts()` → 403; `out_of_quota()` → 429 naming the service
and its account count.

**Test.** Covered end to end by E2 and E3 below, which assert the status code
*and* that the message contains the distinguishing phrase.

### W5 — The move API

**Change.** `PUT /admin/api/providers/{name}/keys/{key}/tenant`, body
`{"tenant": "optimizer"}` or `{"tenant": null}`. Validates the service
exists, edits the one field in the config document, commits through the
store.

**Test.** E5 below.

### W6 — Regressions in existing behaviour

Everything that already worked must keep working. The existing suites cover
it and must stay green: `e2e_conformance`, `e2e_dialect_matrix`,
`e2e_reliability`, `e2e_responses`, `e2e_subscriptions`, `e2e_passthrough`,
`selection_properties`, `fleet`, `store`.

---

## 3 · End-to-end test cases

All in `crates/router-server/tests/e2e_vkey_accounts.rs`, against a real
gateway and a mock provider. Each reads **the credential the mock actually
received**, because that is the only proof of which account served.

Fixture: four accounts — `seat-1`/`seat-2` labelled `agi`, `seat-3` labelled
`kris`, `seat-4` unlabelled — plus a second provider with no labels at all.

| # | Test | Given | Then |
|---|---|---|---|
| **E1** | `a_service_spends_only_its_own_accounts` | a key for `agi`, 30 requests | only `sk-1` and `sk-2` ever reach upstream — never kris's, never the unassigned one |
| **E2** | `two_services_never_share_an_account` | keys for `agi` and `kris` | the two sets are disjoint: agi sees `{sk-1, sk-2}`, kris sees `{sk-3}` |
| **E3** | `a_key_with_no_service_owns_nothing` | a key with no `tenant` | `403`, message contains "owns no account", and **nothing reached upstream** |
| **E4** | `an_unlabelled_pool_serves_everyone` | same key, against the unlabelled provider | `200`, and both accounts get used |
| **E5** | `a_service_does_not_weaken_authentication` | a wrong secret | `401` — labels are admission, not authentication |

### Still to write

| # | Test | Given | Then |
|---|---|---|---|
| **E6** | all of a service's accounts benched | kris's only account out of quota | `429`, message says "all 1 of its accounts are out of quota", and kris's traffic does **not** fall through to agi's accounts |
| **E7** | the move API | `PUT …/keys/seat-4/tenant {"tenant":"kris"}` | 200; kris then reaches `sk-3` **and** `sk-4`; agi still reaches neither |
| **E8** | the move API rejects a ghost service | `{"tenant":"nope"}` | `422`, and the assignment is unchanged |
| **E9** | the relay honours labels | `/passthrough/pool/...` with agi's key | only agi's accounts are used — the relay is not a way around the rule |

E6–E9 are part of this branch. E9 matters most: the relay bypass is the bug
this work uncovered, and a test is the only thing that stops it coming back.

---

## 4 · Order of execution

1. **Get the current state green.** W1–W5 are already written; nothing has
   ever been compiled. Run G1–G5, fix whatever falls out. *No new code until
   this passes.*
2. **Write E6–E9.** Run G4 again.
3. **Full regression.** `cargo test --workspace`, every suite.
4. **Write `COMPLETED.md`** — what changed, why, what is tested, what is
   left.
5. **Commit** the branch.

Step 1 first, deliberately: writing more code on top of code that has never
been compiled is how a small pile of mechanical errors becomes a large one.

---

## 5 · Definition of done

- G1–G6 all pass in the container, from a clean run.
- E1–E9 pass.
- Every pre-existing test suite still passes.
- `COMPLETED.md` records exactly which commands were run and what they
  printed — not a claim that they were run.
- Anything not tested is named as untested. No exceptions, no hedging.
