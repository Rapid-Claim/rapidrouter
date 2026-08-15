# Scaling

The gateway scales horizontally on any substrate — bare VMs, ECS,
Kubernetes, Nomad — because every instance is the same self-contained
binary and the data plane shares nothing
([../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md)).
This page is platform-agnostic; the platform-specific part of a deployment
is never the gateway, only the load balancer and the process manager.

## The shape

```
clients ──► your LB / DNS ──► N × caret-router :8080 ──► providers
                                    │ :9444 (cluster, optional)
                                    └── data-dir per node
```

Two ways to run N instances:

- **Cluster mode** (`managed` config): nodes replicate config/keys/secrets
  among themselves; the console edits the whole fleet from any node.
  The natural choice on VMs and for anyone who wants the console
  read-write. See [clustering.md](clustering.md).
- **Stateless replicas** (`file` config): every instance gets the same
  config file and env secrets from your deploy tooling; instances don't
  talk to each other at all. The natural choice for GitOps and for
  orchestrators that make file+secret distribution trivial.

Both are first-class; they differ only in where config truth lives.

## Load balancer requirements (any LB)

| Requirement | Why |
|---|---|
| Idle/stream timeout ≥ 300 s | long SSE streams; the gateway's 15 s keep-alive comments reset idle clocks |
| No response buffering | SSE must flush per event (`X-Accel-Buffering: no` is set; verify your proxy honors it) |
| Health check `GET /health` | reports draining state for early rotation |
| Connection drain ≥ `drain_timeout_secs` | let in-flight streams finish on scale-in/deploys |
| Stickiness off | stateless data plane; stickiness only skews load |
| WebSocket pass-through | for the realtime roadmap |

## Scaling signal

Scale on **load, not CPU** — an IO-bound proxy saturates connections while
CPU idles:

- Primary: requests-per-instance (LB metric) or `caret_inflight` per node.
- Guardrail: p99 `caret_gateway_overhead_seconds` — the right instance
  count is whatever keeps that histogram flat.
- Sizing: instances are small (0.5 vCPU / 512 MB holds thousands of RPS);
  prefer more small instances over few large ones. Scale out fast, in
  slow — LLM traffic is bursty and instances are cheap.
- The ceiling you'll actually hit first is **provider rate limits**, not
  gateway capacity — watch `caret_key_state` and 429 rates as you grow.

Scale-in semantics: SIGTERM → `/health` flips to draining → in-flight
requests and streams finish within `drain_timeout_secs` → exit. Give your
platform's stop-timeout at least that long. Cluster mode: rate-limit
shares rebalance to the new N automatically; removing a *voter* node
permanently should be followed by `cluster remove` to keep quorum math
right.

## Platform notes (thin by design)

- **VMs**: systemd unit + 3-node cluster + any LB (or DNS-RR/keepalived).
  This is the zero-dependency path end to end.
- **ECS/Fargate**: stateless-replica mode fits naturally — task definition
  injects env secrets, config file baked or mounted; ALB in front
  (idle timeout up, deregistration delay ≥ drain). Auto-scale on
  `RequestCountPerTarget`. Cluster mode works too (service discovery for
  `join`, one small volume per task), but most ECS shops prefer replicas.
- **Kubernetes**: Deployment + Service/Ingress for replicas mode
  (ConfigMap + Secrets), or a StatefulSet + headless Service for cluster
  mode (stable peer DNS, PVC per pod). PodDisruptionBudget ≥ quorum for
  cluster mode.
- **Single box**: the binary. That's the note.
