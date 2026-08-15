# Security

A gateway concentrates every provider credential an organization owns and
sits on the path of every prompt. The security posture is engineered — and
shipped — accordingly.

## Secrets

- Config never contains key material — only references: **`env.*`**
  (resolved from the node's environment) or **`store.*`** (sealed
  XChaCha20-Poly1305 ciphertext in the embedded store; see
  [06-state-and-storage.md](06-state-and-storage.md)).
- In memory they live as `SecretString`: `Debug`/`Display` print `[REDACTED]`,
  memory is zeroized on drop, and the type never implements serialization.
  Leaking a key into a log line is a compile error, not a code-review hope.
- Error bodies returned to clients never contain upstream auth headers or
  key fragments; the error mapper is fuzzed against provider error corpora
  to enforce this.

## Gateway authentication

- Optional gateway keys (`Authorization: Bearer ck-…`) with constant-time
  comparison. Off by default for localhost; `require_auth = true` refuses
  to start without keys when binding non-loopback addresses.
- **Virtual keys** ([../components/virtual-keys.md](../components/virtual-keys.md))
  are the multi-tenant form: scoped per-key access with budgets and rate
  limits. Only `id → BLAKE3(secret)` is ever stored — the store, its
  backups, and the replication stream contain no usable credentials.

## Transport

- Inbound TLS optional (rustls) — most deployments terminate at their edge;
  h2c is supported for internal meshes.
- Outbound: rustls everywhere, certificate verification never disableable
  in release builds, session resumption for latency not at the cost of
  verification.

## Cluster transport & data at rest

- Cluster traffic (port 9444) is mutually authenticated and encrypted; the
  join token authorizes membership and pins the cluster identity. Nothing
  on the cluster port is reachable unauthenticated.
- `store.*` secrets are sealed (XChaCha20-Poly1305, per-node keyfile mode
  0600) before they touch the WAL, snapshots, or the wire — the
  replication stream and backups carry ciphertext only. `env.*` secrets
  never enter the store at all. Threat model stated honestly in
  [06-state-and-storage.md](06-state-and-storage.md).

## Supply chain

The distribution itself is part of the attack surface, and is treated as
such:

- **One static binary** (musl target published); Docker image `FROM scratch`.
  No interpreter, no package manager, no post-install scripts in any
  distribution channel.
- **Dependency discipline**: `cargo-audit` and `cargo-deny` gate CI;
  `cargo-vet` audits new dependencies; the lockfile is committed; the
  dependency count is a reviewed budget, not an accident.
- **Signed releases** (sigstore/cosign) with a published **SBOM** per
  release; reproducible builds are a tracked goal.
- **No telemetry, no phone-home, ever.** The binary makes no network
  connection you didn't configure.

## Isolation properties

- Request bodies are never written to disk; body logging is opt-in,
  sampled, and redaction-filtered even then.
- The passthrough route injects gateway-managed auth but forwards nothing
  else implicitly — no header reflection of internal state.
- Per-request cancellation guarantees a disconnected client cannot hold
  provider capacity.
