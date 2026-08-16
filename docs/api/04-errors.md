# Errors

One internal taxonomy; the *inbound dialect's* native wire shape at the
edge. SDK retry and backoff logic must behave exactly as it would against
the provider directly — that is the compatibility contract.

## Taxonomy → status mapping

| Class | HTTP | Emitted when |
|---|---|---|
| `invalid_request` | 400 | Malformed body; unknown field the dialect can't carry; capability violation (e.g. `n > 1` to a target without it) — message names the exact parameter |
| `authentication` | 401 | Missing/invalid gateway key |
| `permission` | 403 | Key not scoped for the requested provider/model |
| `not_found` | 404 | Unknown model/alias, unknown passthrough provider |
| `payload_too_large` | 413 | Body over `max_body_size` |
| `rate_limited` | 429 | Gateway/virtual-key limit hit, or all upstream keys throttled — `retry-after` always present |
| `insufficient_quota` | 429 | Virtual-key budget exhausted ([../components/virtual-keys.md](../components/virtual-keys.md)) |
| `upstream_error` | 502 | Provider returned 5xx / malformed response after retries and fallbacks were exhausted |
| `no_capacity` | 503 | Breaker open everywhere / provider semaphores saturated, no fallback available |
| `timeout` | 504 | Connect, first-byte, or total deadline exceeded on the last candidate |

## Wire shapes

`/v1` returns OpenAI-format errors:

```json
{ "error": { "message": "…", "type": "rate_limit_error", "code": "rate_limited", "param": null } }
```

`/anthropic` returns Anthropic-format errors (`{"type": "error", "error":
{…}}`); `/genai` returns Google-format error objects. Mid-stream failures
arrive as the dialect's terminal error event
([03-streaming.md](03-streaming.md)).

## Provider detail without leakage

The upstream provider's own error text and status are preserved for
diagnosis — in logs, and in the response body's `metadata` (provider name,
upstream status) — but auth headers and key material can never appear: the
error mapper is fuzzed against captured provider error corpora to prove it.

## Retry semantics (what the gateway already did for you)

By the time a client sees an error, the router has already advanced through
its candidates: remaining keys of the provider, then each fallback target
([../components/router.md](../components/router.md)). Response headers say
what happened:

| Header | Meaning |
|---|---|
| `x-rapid-provider`, `x-rapid-model` | who actually served (or last failed) |
| `x-rapid-attempts` | total candidates tried |
| `retry-after` | propagated from upstream 429s, or computed from gateway limits |

Client-side retries on 429/5xx therefore remain safe and meaningful — each
retry re-enters routing with fresh health state.
