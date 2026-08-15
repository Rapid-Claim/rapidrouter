# Clustering

Cluster mode is the same binary discovering peers — not a different
deployment. One node is a cluster of one; adding nodes is one command per
node. No external coordination service, no database, no shared disk.

## Forming a cluster

```bash
# box 1 — first boot bootstraps a single-node cluster
caret-router --data-dir /var/lib/caret-router

# print the join token (also shown in the console, Cluster page)
caret-router cluster token
# caret-join-1.eyJjbHVzdGVyIjoi…

# boxes 2 and 3 — same binary, same config style
caret-router --data-dir /var/lib/caret-router \
  --join box1.internal:9444 \
  --cluster-token env.CARET_CLUSTER_TOKEN
```

Joining streams a snapshot of the replicated store (config, virtual keys,
sealed secrets) to the new node, adds it as a voter, and it starts serving
immediately after apply. Membership changes use joint consensus — no
downtime, no manual quorum arithmetic.

Equivalent config-file form:

```toml
[cluster]
listen = "0.0.0.0:9444"
join = ["box1.internal:9444", "box2.internal:9444"]   # any live subset
token = "env.CARET_CLUSTER_TOKEN"
```

## Ports

| Port | Purpose |
|---|---|
| `8080` | data plane + console (as ever) |
| `9444` | cluster: Raft replication, join, peer scatter-gather APIs |

All cluster traffic is mutually authenticated and encrypted; the join
token both authorizes membership and pins the cluster identity. Keep 9444
on your internal network.

## What clustering gives you

- **One config everywhere**: a change on any node (console, API, CLI)
  commits through consensus and applies on all nodes — the console works
  identically no matter which node serves it.
- **Secrets entered once**: `store.*` secrets replicate as ciphertext;
  a new node can serve immediately without any per-node secret setup
  (env-based secrets remain per-node by nature).
- **Fleet views**: the console merges usage and health from all peers.
- **Self-adjusting limits**: per-node rate-limit shares track the live
  member count automatically
  ([../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md)).

And what it deliberately does not do: cluster nodes do not proxy data-plane
traffic for each other, and there is no built-in virtual IP — put your load
balancer, DNS-RR, or keepalived in front, pointing at all nodes' `:8080`.

## Failure behavior

The invariant: **quorum loss degrades config writes, never traffic.**
Every node keeps serving from its last applied state.

| Nodes | Tolerates (for config writes) | Notes |
|---|---|---|
| 1 | 0 | the default single-box experience |
| 2 | 0 | replication yes, fault-tolerant writes no — run 1 or 3 |
| 3 | 1 | the recommended small fleet |
| 5 | 2 | large fleets |

| Scenario | Behavior |
|---|---|
| Leader dies | Election in ~ms–s; data plane unaffected; writes retry transparently |
| Minority partition | Minority nodes serve traffic on last state, reject config writes with a clear error; heal → catch up from log/snapshot |
| Node disk lost | Rejoin with `--join`; store re-syncs by snapshot; only that node's local usage history is gone |
| Whole cluster restart | Nodes recover from their WALs; no external dependency in the recovery path |
| Rolling binary upgrade | Standard: drain (SIGTERM) one node at a time; store format is versioned with N−1 compatibility |

## Operating commands

```bash
caret-router cluster status      # members, roles, lag, applied config version
caret-router cluster token       # print/rotate join token
caret-router cluster remove <id> # remove a dead node from membership
caret-router config export       # replicated store → TOML on stdout
caret-router secret set <name>   # prompt + seal + replicate a store.* secret
```

Everything above is also visible on the console's Cluster page — which is
served by every node, so "which box do I look at" is never a question.

## Choosing your topology

| Deployment | Recommendation |
|---|---|
| One VM / laptop | Default single node; nothing to configure |
| A few VMs, no platform | 3 nodes, `managed` config mode, `store.*` secrets, LB or DNS in front |
| Orchestrated (ECS, Kubernetes, Nomad) | Either: stateless replicas in `file` config mode (platform injects env secrets, config ships with deploys) — clustering optional; or `managed` mode with a persistent volume per replica and peer discovery via the platform's DNS |
| GitOps shops | `file` mode everywhere; console read-only; config changes are commits |
