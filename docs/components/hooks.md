# Middleware & Hooks

Two extension seams, both compiled in — no dynamic loading in the core
(a sandboxed plugin runtime is a roadmap item that attaches at the same
seam).

## 1 · tower layers — HTTP-level, dialect-agnostic

The fixed stack in `router-server`, statically composed (each layer a
concrete type; no boxed futures on the happy path):

```
TraceLayer → RequestIdLayer → AuthLayer → RateLimitLayer? → MetricsLayer → handler
```

Anything that needs only headers, status, and timing belongs here.

## 2 · The `Hook` trait — gateway-aware

```rust
pub trait Hook: Send + Sync + 'static {
    /// Before upstream dispatch; may adjust the resolved route or reject.
    fn pre(&self, ctx: &mut RequestCtx) -> HookOutcome { HookOutcome::Continue }

    /// After completion — including streams, with accumulated usage.
    fn post(&self, ctx: &RequestCtx, res: &ResponseSummary) {}
}
```

- Registered at startup; executed sequentially; an empty registry costs one
  branch.
- `post` failures are logged, never fatal to the request.
- `RequestCtx` exposes the resolved route, model, dialects, and **lazy**
  parsed-body access — a hook that never touches the body never forces a
  parse, preserving the splice fast path.
- `ResponseSummary` carries status, usage (accumulated by the stream
  translator for streaming responses), timing, and the fallback trail.

## Built-in hooks

| Hook | Purpose |
|---|---|
| Usage & cost accounting | provider-reported tokens × config price table → metrics + per-request log field |
| Request/response logging | sampled; bodies opt-in only; redaction always applied |
| Virtual-key enforcement | scope check, per-key token buckets, budget cutoff ([virtual-keys.md](virtual-keys.md)) |
| Gateway rate limiting | per-gateway-key token buckets (input+output tokens), atomically enforced |

Virtual-key governance is itself built on this seam — proof that the
extension point carries real features, not just logging.
