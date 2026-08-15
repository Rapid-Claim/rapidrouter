# Running a fleet

Every node runs the same binary, holds no state worth keeping, and can be
destroyed at any moment. There is nothing to join, no membership to
manage, no leader, and no quorum. A node is a member of a fleet because
it is pointed at the same control-plane store as the others — that is the
whole of it.

This is what makes the router fit an autoscaling group, an ECS service,
or a Kubernetes `Deployment`: `desired_count` is the only knob, and
scaling it in either direction needs no coordination.

## Choosing a store

| Backend | Use it when | Needs |
|---|---|---|
| `file` | One box, a laptop, or several nodes on a shared volume | Nothing |
| `s3` | You are already in AWS and writes are rare | A bucket |
| `dynamodb` | You want the lowest write latency and per-request billing | A table |

All three implement the same contract and pass the same conformance
suite. Swapping between them is a config change and a data copy.

### File, including on a shared volume

```bash
caret-router --store-path /mnt/shared/caret/store.json --data-dir /var/lib/caret-router
```

`--store-path` separates *where the fleet's document lives* from *where
this node keeps its own scratch*. Point several nodes at one path on a
shared volume — EFS, an NFS mount — and they form a fleet with no AWS at
all: writes compare-and-swap on the version, and heartbeats are files
under `nodes/` beside the document.

The fallback encryption key is written next to the document rather than
in the data dir, so nodes sharing the document share the key
automatically and secrets work without any further setup.

### S3

```toml
[store]
backend = "s3"
bucket  = "acme-caret-router"
prefix  = "prod/"          # optional
```

The document is one object, `prod/store.json`, written with S3's
conditional `If-Match`/`If-None-Match` so two nodes editing at once
produce a visible conflict rather than a lost update. Heartbeats are
small objects under `prod/nodes/`.

IAM needs `s3:GetObject`, `s3:PutObject`, `s3:DeleteObject`, and
`s3:ListBucket` on that prefix.

### DynamoDB

```toml
[store]
backend = "dynamodb"
table   = "caret-router"
```

Create the table with a composite key — partition `pk` (string), sort
`sk` (string) — and enable TTL on `expires_at`. On-demand billing is
right: the traffic is a handful of small reads per node per interval.

```bash
aws dynamodb create-table \
  --table-name caret-router \
  --attribute-definitions AttributeName=pk,AttributeType=S AttributeName=sk,AttributeType=S \
  --key-schema AttributeName=pk,KeyType=HASH AttributeName=sk,KeyType=RANGE \
  --billing-mode PAY_PER_REQUEST
aws dynamodb update-time-to-live --table-name caret-router \
  --time-to-live-specification "Enabled=true,AttributeName=expires_at"
```

IAM needs `dynamodb:GetItem`, `PutItem`, `DeleteItem`, and `Query` on the
table. TTL is a garbage collector for the heartbeats of nodes that died
without departing; it is deliberately not the liveness mechanism, because
DynamoDB may take minutes to actually delete.

## The master key

Stored secrets are encrypted before they reach the store, so the bucket
or table holds ciphertext. Every node must therefore hold the same key:

```bash
caret-router master-key            # prints a fresh key
```

Set it as `CARET_MASTER_KEY` on every node, from Secrets Manager, SSM, or
whatever your platform provides. A node pointed at a shared store without
it **refuses to start**, rather than sealing secrets the rest of the
fleet cannot read — that failure is silent and looks like a bad API key.

The single-node `file` backend mints a key beside its data if you do not
supply one, because there is nobody to disagree with.

## What propagation actually costs

There is no replication. Each node polls the store and adopts what it
finds:

| Setting | Default | What it controls |
|---|---|---|
| `refresh_interval_secs` | 3 | How long a config change takes to reach the fleet |
| `heartbeat_interval_secs` | 5 | How often a node announces itself |
| `liveness_window_secs` | 15 | How long after its last heartbeat a node still counts |

So a config edit is live on the node you made it on immediately and
everywhere else within `refresh_interval_secs`. A node that dies stops
counting toward rate-limit shares within `liveness_window_secs`; one that
shuts down cleanly stops counting at once.

Steady-state cost per node is two small requests every few seconds. On
DynamoDB on-demand that is cents per month for any realistic fleet.

These three settings are **node-local**, and they have to be: a node
needs them before it can read anything, so they cannot come from the
document they configure access to. That means:

* Set them identically on every node. Give every task the same config
  file or the same environment; an ECS task definition or a Kubernetes
  ConfigMap does this for you.
* A node whose `liveness_window_secs` is shorter than another node's
  `heartbeat_interval_secs` will flap that node in and out of its count,
  and rate-limit shares will oscillate. Within a single node this is
  rejected at startup; across nodes nothing can check it for you.

Everything else — providers, keys, limits, budgets — comes from the
shared document, so a node needs no config file at all once the fleet
has one. That is what `caret-router --store-path ...` with no `--config`
does: it reads the fleet's configuration and starts serving it.

## Rate limits and the fleet size

A key's `rpm` is a fleet-wide number. Each node enforces its share —
the limit divided by the number of live nodes — so the total stays
roughly right as the fleet scales without anyone editing a config.

This is an approximation, and worth understanding before you rely on it:

* Between a node dying and the liveness window expiring, the fleet is
  briefly enforcing a limit divided by a count that is too high, so the
  effective ceiling is under the configured one.
* Immediately after a scale-out, before the new node's first heartbeat is
  seen, the ceiling is briefly over.
* Traffic is assumed to be spread evenly. Behind a load balancer that
  does not do that, a node can exhaust its share while others idle.

If you need an exact global limit, this design will not give you one; it
would require a shared counter on the request path, which is a cost the
data plane deliberately does not pay.

## When the store goes down

Nothing stops serving. The data plane reads the config and key table from
memory and never touches the store on the request path, so an outage in
S3 or DynamoDB affects:

* **Config and key writes** — refused with `503` and a message saying so.
* **Propagation** — nodes keep the last document they read.
* **Fleet accounting** — heartbeats fail, so the console's node list goes
  stale and shares stop rescaling.

The console's Fleet page shows the store as unreachable. No action is
needed on the nodes themselves; they recover when the store does.

## Deploying

Nodes are interchangeable, so a rolling deploy needs no ordering and no
draining beyond the usual connection drain. A stopping node removes its
own heartbeat, which returns its rate-limit share to the fleet
immediately.

Because the configuration is shared, every node also gets the same
`[server] port`. On separate hosts that is what you want. To run two
nodes on one host, override it per node with `--port` (or `CARET_PORT`);
nothing else needs to differ.

Two things do need care:

* **Roll out `CARET_MASTER_KEY` before the config that uses new secrets.**
  A node that cannot decrypt a secret refuses to adopt the config naming
  it, and logs why. It keeps serving the previous one, so this degrades
  rather than breaks — but the fleet will be split across two configs
  until the rollout finishes.
* **Do not point two different environments at one store.** There is no
  namespacing beyond the bucket prefix or table name.

## Inspecting a fleet

```bash
caret-router fleet --store-backend dynamodb --store-table caret-router
```

prints the store, the document version, and every node heartbeating
against it. The console's Fleet page shows the same thing and refreshes
live. Both work from any node, because every node sees the same store.
