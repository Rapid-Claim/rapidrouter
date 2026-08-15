# Benchmarking

The overhead claim is the product; the rigs that verify it live in this
repo, run in CI, and are reproducible by anyone.

## The three rigs

### 1 · Criterion micro-benches (`benches/micro/`)
Per-stage: JSON splice, borrowed translation per adapter, SSE codec, key
selection, breaker check, error mapping. CI fails on a >10 % regression
against stored baselines.

### 2 · End-to-end overhead rig (`benches/e2e/`)
caret-router in front of a **local mock provider** (canned responses in
~zero time), loaded by a fixed-RPS generator. Reported: added p50/p99/p999
versus hitting the mock directly — that delta *is* the gateway overhead.
Runs at 500 / 1 000 / 5 000 RPS on a pinned runner class; results published
per release.

Honesty rules: fixed-RPS (not closed-loop) load so coordinated omission
doesn't flatter tails; the mock's own jitter measured and subtracted;
translated and passthrough paths reported separately.

### 3 · Streaming rig (`benches/stream/`)
Mock SSE provider emitting N chunks at realistic cadence. Reported: TTFT
delta, per-chunk overhead distribution, and behavior with slow readers.

## The soak

24 hours at moderate sustained load, mixed request shapes and sizes,
periodic config reloads and provider-failure injections. Charted per
release: RSS over time (must be flat), p99 overhead over time (must be
flat), fd counts, breaker recovery times. Leaks and drift are release
blockers.

## Targets (enforced)

| Number | Target |
|---|---|
| Added overhead p50, passthrough | < 10 µs |
| Added overhead p50, translated | < 20 µs |
| Added overhead p99 | < 100 µs |
| Per-chunk overhead, raw forward | < 100 ns |
| Per-chunk overhead, translated | < 1 µs |
| RSS over 24 h soak | flat |

## Running locally

```bash
cargo bench                                   # micro benches
cargo run -p rig-e2e -- --rps 1000 --secs 60  # e2e overhead
cargo run -p rig-stream -- --chunks 200       # streaming
```

Profiles (flamegraphs from rig 2) are committed under `docs/perf-notes/`
per release so a regression is diagnosable by diffing two SVGs.
