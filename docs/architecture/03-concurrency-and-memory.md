# Concurrency & Memory

## Task model: one request, one task, no hops

Each inbound request runs as a single tokio task from accept to final byte.
There are no internal queues, no worker pools, no channel relays between
stages — every hand-off would cost scheduler latency and reorder tail
behavior. The borrow checker guarantees the aliasing safety that queue-based
designs buy with copies.

Cancellation falls out of drop semantics: a client disconnect drops the
task, which drops the upstream hyper body (RST_STREAM / connection close), releases
the semaphore permit, and runs breaker bookkeeping — no reaper, no leak.

## Backpressure: explicit, per provider

- Each provider has a `tokio::sync::Semaphore` bounding in-flight upstream
  calls (`max_concurrency`, default 512).
- Permit acquisition happens at routing time. A saturated provider fails
  fast to the fallback chain, or returns an honest 429 with `retry-after` —
  it never queues invisibly.
- A global `ConcurrencyLimit` layer exists as a safety valve, tuned high.

## Shared state: read-mostly, lock-free

| State | Mechanism | Cost per request |
|---|---|---|
| Routing table / config snapshot | `ArcSwap<RoutingTable>` | one atomic load |
| Key health, breaker state | atomics (state + windowed counters) | one load per check |
| Key selection | precomputed Vose alias table | two array reads + RNG |
| Rate/usage counters | sharded atomics | one fetch_add |

Writers (hot reload, breaker transitions) are rare and never block readers:
reload builds a complete new table and swaps it in; in-flight requests keep
their old snapshot to completion.

The correctness bar is explicit: limiter and accounting invariants hold
under arbitrary interleaving, verified with loom model checking and
proptest — racing requests cannot bypass a limit or double-count usage.

## Allocation discipline

- **Bodies are `Bytes` ropes.** Passthrough forwards the inbound buffer's
  refcounted slices; the spliced `model` edit reuses the untouched regions.
  A multi-megabyte image part is pointer arithmetic, not memcpy.
- **Translation borrows.** Typed request structs borrow `&str`/`Cow` from
  the source buffer; unknown fields ride along as `&RawValue` and re-emit
  verbatim. Output goes to one pre-sized `BytesMut` (`input.len() + 256`).
- **Scratch reuse.** Translation scratch buffers are thread-local and
  recycled; steady-state translation allocates near-zero.
- **No `String` at layer boundaries.** UTF-8 validation happens once,
  inside the JSON parser.
- **Allocator: mimalloc**, chosen on bench evidence for tail latency.
- **Flat memory is a tested property.** The 24-hour soak rig charts RSS;
  CI fails on drift. Multi-tenant key state costs a few atomics per key,
  so ten thousand virtual keys cost megabytes, not gigabytes.

## Runtime tuning

- Worker threads = physical cores (configurable). Nothing on the request
  path calls `spawn_blocking`.
- Translation of unusually large bodies (> 256 KB) processes in chunks with
  cooperative yields so small requests sharing a worker keep their tail
  latency.
- TCP_NODELAY on; vectored writes for header+body; one flush per SSE event.

## Build configuration

Release builds use `lto = "fat"`, `codegen-units = 1`, and PGO against the
end-to-end rig. `#[inline]` appears only with benchmark evidence.
