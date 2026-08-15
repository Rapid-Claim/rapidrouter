# Observability

Rule: insight is never bought with hot-path latency. Recording is
atomic-cheap, cardinality is bounded, and anything heavier is opt-in.

## Metrics — `GET /metrics` (Prometheus)

Labels are bounded: `provider`, `model`, `status_class`, `dialect` — never
raw keys, never caller identifiers.

| Metric | Type |
|---|---|
| `caret_requests_total{provider,model,status_class,dialect}` | counter |
| `caret_gateway_overhead_seconds` | histogram — the headline number, µs-resolution buckets |
| `caret_ttft_seconds{provider,model}` | histogram |
| `caret_upstream_duration_seconds{provider}` | histogram |
| `caret_stream_chunks_total`, `caret_stream_chunk_overhead_seconds` | counter, histogram |
| `caret_tokens_total{provider,model,kind=prompt\|completion\|cached}` | counter |
| `caret_cost_usd_total{provider,model}` | counter (price-table derived) |
| `caret_key_state{provider,key_name,state}` | gauge — breaker closed/open/half-open |
| `caret_fallbacks_total{from,to}`, `caret_retries_total`, `caret_dropped_params_total{param,provider}` | counters |
| `caret_inflight{provider}` | gauge |
| `caret_cluster_members`, `caret_cluster_is_leader`, `caret_cluster_applied_version` | gauges (cluster mode) |

## Tracing

- `tracing` spans: `request → route → attempt(n) → upstream → translate`.
  With no subscriber, spans compile to a branch.
- Optional OTLP export (`[observability.otlp]` in config), sampled (default
  1 %); span attributes mirror metric labels plus request id.
- `x-request-id`: honored inbound or generated (UUIDv7), returned on every
  response, attached to every log line.

## Logging

- JSON lines to stdout (12-factor); `--dev` for human-pretty output.
- One INFO line per request: id, dialect, provider/model (+ fallback trail),
  status, overhead µs, TTFT ms, tokens, cost.
- **Bodies are never logged by default**; `log_bodies = "sampled"|"on"` is
  explicit, and redaction applies even then. `SecretString` makes key
  leakage a type error.

## The receipt headers

Client-visible observability on every response:

| Header | Meaning |
|---|---|
| `x-request-id` | correlation id |
| `x-caret-provider`, `x-caret-model` | who actually served (fallbacks disclosed) |
| `x-caret-attempts` | candidates tried |
| `x-caret-overhead-us` | this request's measured gateway overhead — the promise, auditable per request |
