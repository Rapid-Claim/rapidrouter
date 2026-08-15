# State & Storage

Two rules govern the storage design:

1. **The binary is the whole system.** One box, one binary, no external
   services: the gateway runs, the console works, configuration changes
   persist. Three boxes, the same binary: they form a cluster and
   everything still works. Nothing else to deploy — no database, no object
   store, no secrets manager *required* (all of them optionally pluggable).
2. **The data plane never depends on the control plane.** Routing traffic
   requires only the local, already-applied state snapshot. Cluster
   consensus, disk, even the entire control plane can be degraded and
   requests keep flowing.

## The state inventory

| State | Nature | Home |
|---|---|---|
| Config document (providers, aliases, fallbacks, limits, virtual keys) | small, rarely written, must converge | **embedded replicated store** (below) — or a plain file in `file` mode |
| Provider secrets | sensitive | `env.*` references (your platform injects), or `store.*` references (entered once, encrypted at rest, replicated) |
| Breaker / health / in-flight | ephemeral, hot | in-memory per node, deliberately never shared |
| Usage & spend records | append-only events | local JSONL partitions in the data dir; cluster-wide views by scatter-gather; optional external sink |
| Metrics | time series | your Prometheus — `/metrics` per node |
| Console assets | static | embedded in the binary |

## The embedded replicated store

The control-plane state lives in a Raft-replicated document store built
into the binary (`router-cluster` crate; WAL + snapshots under
`--data-dir`, default `/var/lib/caret-router`).

**A single node is simply a cluster of one.** Same code path, no special
mode: the Raft log has one voter, writes commit instantly, and the
overhead is a WAL append on config changes — i.e., almost never. This is
what makes "spin up a binary in one box and it works" and "three boxes
form a cluster" the *same* product rather than two products.

What is replicated: the config document (versioned), virtual keys,
`store.*` secrets (as ciphertext), and console/admin settings. What is
not: anything ephemeral or high-write — breakers, in-flight counts, and
usage events stay node-local by design.

### Write path
A change (console, admin API, or CLI) is validated **totally** first, then
committed through the leader to a majority, then applied on every node via
the same atomic routing-table swap as a file reload. Any node accepts a
write and forwards it to the leader transparently. Every write carries the
version it was based on — concurrent edits conflict visibly instead of
last-write-wins.

### Read path
Every node holds a full replica; data-plane reads are the usual lock-free
snapshot loads. **Loss of quorum stops config *writes*, never traffic**:
nodes continue serving from their last applied state until the cluster
heals.

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
