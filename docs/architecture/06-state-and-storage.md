# State & Storage

Two rules govern the storage design:

1. **A node holds nothing worth keeping.** Everything that must survive
   lives in a control-plane store the node points at; everything on the
   node itself is a cache it can rebuild by reading that store once. A
   task can be destroyed mid-request and replaced by one that has never
   run, and the replacement is indistinguishable from the original.
2. **The data plane never depends on the control plane.** Routing traffic
   requires only the in-memory snapshot. The store can be unreachable,
   the disk can be gone, and requests keep flowing.

The first rule is what the second one buys. Because reads never touch the
store, the store is allowed to be a remote service that occasionally
fails — and once that is true, it can be S3 or DynamoDB instead of
something running on the node.

## The state inventory

| State | Nature | Home |
|---|---|---|
| Config document (providers, aliases, fallbacks, limits, virtual keys) | small, rarely written, must converge | **the control-plane store** (below) — or a plain file in `file` mode |
| Provider secrets | sensitive | `env.*` references (your platform injects), or `store.*` references (entered once, encrypted under the fleet's master key) |
| Breaker / health / in-flight | ephemeral, hot | in-memory per node, deliberately never shared |
| Usage & spend records | append-only events | local JSONL partitions in the data dir; fleet-wide views by scatter-gather; optional external sink |
| Metrics | time series | your Prometheus — `/metrics` per node |
| Console assets | static | embedded in the binary |

## The control-plane store

All control-plane state is **one small JSON document** — config text,
virtual keys, sealed secrets, settings — plus a version. Where that
document lives is a backend choice behind one trait
(`router-store::backend::ControlPlane`):

| Backend | Document | Concurrency |
|---|---|---|
| `file` | `store.json` under `--data-dir` | version re-read under a rename |
| `s3` | one object in a bucket | `If-Match` / `If-None-Match` |
| `dynamodb` | one item in a table | `ConditionExpression` on `version` |

Adding Postgres or anything else means implementing that trait; nothing
above it changes.

**There is no consensus.** Ordering comes from the backend's conditional
write, not a replicated log. For a document that changes when a human
edits it, that is the right trade — and it is what removes per-node
state, node identity, membership, quorum, and the join flow all at once.

What lives in the document: the config (versioned), virtual keys,
`store.*` secrets as ciphertext, and console settings. What does not:
anything ephemeral or high-write — breakers, in-flight counts, and usage
events stay node-local by design.

### Write path

A change (console, admin API, or CLI) is validated **totally** first, then
written back with a compare-and-swap on the version it was based on. A
losing write gets a visible conflict instead of a silent overwrite: if the
caller supplied the version it was looking at, the conflict is reported to
them; if it did not, the node re-reads and retries. Any node accepts a
write — there is no leader to forward to.

### Read path

Reads never leave the process. Each node keeps the document in memory and
refreshes it on a timer (default 3s), swapping the routing table
atomically the same way a file reload does. That is the whole propagation
mechanism: no leader, no push, no replication stream.

A config that fails to build on a particular node — most often an `env.*`
or `store.*` reference it cannot resolve mid-rollout — is **not adopted**.
The node logs why and keeps serving the last good one, rather than taking
itself down over a problem it did not cause.

**Store unavailable stops config *writes*, never traffic.** Nodes serve
from the last document they read until it returns.

### Secrets and the master key

Secrets are sealed with XChaCha20-Poly1305 before they reach the store,
so the bucket or table holds only ciphertext. The key is fleet-wide and
supplied out of band as `CARET_MASTER_KEY`; a node pointed at a shared
store without one refuses to start, because sealing under a key no other
node holds fails silently and presents as a bad API key.

### Liveness

Nodes announce themselves by writing a heartbeat to the same store and
count the recent ones to size rate-limit shares. A node that stops
beating ages out; one that shuts down cleanly removes its own heartbeat
immediately. That is the entirety of membership — see
[../operations/fleet.md](../operations/fleet.md).

## Config modes

| Mode | Source of truth | Console | For whom |
|---|---|---|---|
| `managed` (default) | the replicated store; an optional config file acts as **first-boot seed** | read-write | "run the binary, configure in the browser, it persists and replicates" |
| `file` | the config file, hot-reloaded; store not used for config | read-only | GitOps / immutable infra — your deploy tool distributes the file to every node |

`caret-router config export` writes the managed store's current document
out as a TOML file at any time — migration between modes (and disaster
recovery) is a file copy, never a lock-in.

## Secrets without a secrets manager

Both reference forms work everywhere, including mixed:

- **`env.OPENAI_API_KEY`** — the value comes from the node's environment.
  Best when a platform (ECS, Kubernetes, systemd credentials) injects
  secrets; the gateway never stores anything.
- **`store.openai_key`** — the value was entered once via console or
  `caret-router secret set openai_key`, is encrypted with
  XChaCha20-Poly1305 under a data-encryption key generated at first boot
  (`data-dir/node.key`, mode 0600), and replicates as ciphertext.
  Decrypted only in memory, into the same `SecretString` type as env
  secrets — same redaction guarantees end to end.

Honest threat model: `store.*` protects the store's files, backups, and
replication stream from disclosure; it does not protect against root on a
running node (nothing software-only does). The keyfile can optionally be
wrapped by an external KMS for deployments that have one — a convenience,
never a requirement.

## Usage & analytics

Each node appends usage records (id, key, provider/model, tokens, cost,
latency, status) to compressed, date-partitioned JSONL in its data dir —
one ring-buffer write on the hot path, batched flush off it. Then:

- **Cluster-wide views**: the console scatter-gathers summaries from every
  peer over the cluster port and merges — no shared storage involved.
- **Retention**: `usage.retention_days` pruned locally by the binary.
- **Optional sinks**: `usage_sink = "s3://…"` (any S3-compatible store)
  ships partitions out for long-term warehouse queries. Optional, additive.

A lost node loses at most its unflushed ring (seconds) and its local
history since the last sink shipment — the documented cost of "no
database"; deployments that can't accept it configure the sink.

## Rate limits and budgets, cluster-honest

- **Rate limits**: per-node atomic token buckets. In a cluster, each
  node's share is `limit / N` where **N is the live member count from
  cluster membership** — shares rescale automatically as nodes join,
  leave, or fail; nobody edits configs when the fleet resizes.
- **Spend budgets**: enforced from usage aggregation (local + peer
  summaries), cutoff lag bounded by the flush/gossip interval — the right
  semantics for budgets.
- **Strict global limits** (exactly-N-RPS fleet-wide) would put consensus
  on the hot path; permanently out of scope. The `limit / N` shares plus
  periodic idle-share rebalancing (leader redistributes unused quota,
  seconds-lagged) cover real-world needs without it.

## On disk

```
/var/lib/caret-router/
├── node.key            # DEK + node identity (0600)
├── raft/               # WAL + snapshots (the replicated store)
└── usage/dt=2026-08-15/*.jsonl.zst
```

Back up = copy the directory (or `config export` + your secret refs).
Everything is small: the store snapshot is kilobytes; usage dominates and
is prunable.
