# Deployment

## Artifacts

- **Static binary** per platform (musl builds for Linux) — no runtime, no
  post-install scripts.
- **Docker image** built `FROM scratch`: the binary and CA certificates,
  nothing else.
- Every release: sigstore/cosign signatures + SBOM
  ([../architecture/05-security.md](../architecture/05-security.md)).

## Running

```bash
rapid-router                              # zero-config: :8080, env-var provider discovery,
                                          #   data dir at /var/lib/rapid-router (fallback ~/.rapid-router)
rapid-router --config rapid-router.toml   # seed (managed mode) or source of truth (file mode)
rapid-router check rapid-router.toml      # validate only (CI)
rapid-router --watch                      # file mode: hot reload on change (else: SIGHUP)
rapid-router --join box1:9444 --cluster-token env.RAPID_CLUSTER_TOKEN   # add this box to a cluster
rapid-router config export                # store → TOML on stdout
rapid-router secret set openai_key        # seal a store.* secret
```

```bash
docker run -p 8080:8080 \
  -v rapid-data:/var/lib/rapid-router \
  -v $PWD/rapid-router.toml:/etc/rapid-router.toml \
  -e OPENAI_API_KEY -e ANTHROPIC_API_KEY \
  ghcr.io/rapid/rapid-router --config /etc/rapid-router.toml
```

The volume holds the embedded store and usage partitions; pure `file`-mode
replicas that ship usage to an external sink can run without it.

## Lifecycle

- **Startup** binds the port only after total config validation; failure
  modes are exit-with-pathed-error, never half-up.
- **`SIGHUP`** (file mode): atomic config reload — old config retained on
  any error. Managed-mode changes arrive via the store and apply the same
  way ([../components/config.md](../components/config.md)).
- **`SIGTERM`**: graceful drain — stop accepting, let in-flight requests
  and streams finish within `drain_timeout_secs` (default 30), then exit.
  `/health` flips to draining immediately so load balancers rotate early.

## Sizing & scaling

- CPU-light by design: the gateway adds microseconds of compute per
  request; capacity is usually bounded by connection counts and provider
  latencies, not cores. Start with 2 vCPUs; scale on p99 overhead and
  `rapid_inflight`.
- Memory is flat and small: buffers scale with concurrent request bodies;
  per-key state is atomics. The soak rig
  ([benchmarking.md](benchmarking.md)) charts 24-hour RSS per release.
- **Horizontal scaling is built in**: run more copies of the binary —
  either as stateless replicas (`file` config mode, your tooling
  distributes config and secrets) or in **cluster mode**, where nodes
  replicate config, keys, and secrets among themselves with no external
  services ([fleet.md](fleet.md)). Signals and LB requirements:
  [scaling.md](scaling.md); state model:
  [../architecture/06-state-and-storage.md](../architecture/06-state-and-storage.md).

## Fronting

Terminate public TLS at your edge (or enable inbound rustls). Ensure the
proxy in front (a) does not buffer SSE responses (`X-Accel-Buffering: no`
honored; keep-alive comments emitted on idle), and (b) passes request
bodies up to your configured `max_body_size`.

## Endpoints for ops

| Endpoint | Use |
|---|---|
| `GET /health` | liveness/readiness (reports draining state) |
| `GET /metrics` | Prometheus scrape |
| `/console`, `/admin/api/*` | embedded console + admin API (only when admin keys are configured) |
| — | no second port: nodes never talk to each other, only to the shared store ([fleet.md](fleet.md)) |
