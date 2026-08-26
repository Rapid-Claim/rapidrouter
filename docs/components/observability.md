# Observability

Rule: insight is never bought with hot-path latency. Recording is
atomic-cheap, cardinality is bounded, and anything heavier is opt-in.

## Metrics — `GET /metrics` (Prometheus)

Labels are bounded: `provider`, `model`, `status_class`, `dialect` — never
raw keys, never caller identifiers.

| Metric | Type |
|---|---|
| `rapid_requests_total{provider,model,status_class,dialect}` | counter |
| `rapid_gateway_overhead_seconds` | histogram — the headline number, µs-resolution buckets |
| `rapid_ttft_seconds{provider,model}` | histogram |
| `rapid_upstream_duration_seconds{provider}` | histogram |
| `rapid_stream_chunks_total`, `rapid_stream_chunk_overhead_seconds` | counter, histogram |
| `rapid_tokens_total{provider,model,kind=prompt\|completion\|cached}` | counter |
| `rapid_cost_usd_total{provider,model}` | counter (price-table derived) |
| `rapid_key_state{provider,key_name,state}` | gauge — breaker closed/open/half-open |
| `rapid_fallbacks_total{from,to}`, `rapid_retries_total`, `rapid_dropped_params_total{param,provider}` | counters |
| `rapid_inflight{provider}` | gauge |
| `rapid_cluster_members`, `rapid_cluster_is_leader`, `rapid_cluster_applied_version` | gauges (cluster mode) |

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

## Caller-supplied dimensions

A gateway knows what it served and what it cost; it does not know *which
piece of work* the call belonged to. Callers supply that, and the
gateway lifts it onto every usage record so a log can be narrowed to one
workflow, one chart, one agent, or one pipeline stage.

Send it under `metadata` in the request body. Both shapes are read,
because both are in use:

```jsonc
// Flat — clients talking to the gateway directly.
{"metadata": {"workflow_id": "WORKFLOW_HCC_CONFIRMED", "chart_id": "…",
              "service": "agentic_dag_coder", "agent": "icd_coder"}}

// Nested — the Langfuse shape LiteLLM-based clients emit.
{"metadata": {"trace_metadata": {"workflow_id": "…", "chart_id": "…",
                                 "event_processing_tag": "ICD_EXTRACTION"}}}
```

`X-Org-Id`, `X-Chart-Id` and `X-Workflow-Id` headers fill in a dimension
the body omitted. **The body always wins** — a header is a fallback, not
an override, so a client can send both without one shadowing the other.

Which keys are kept is `usage.trace_keys`, defaulting to `workflow_id`,
`chart_id`, `org_id`, `service`, `agent`, `stage`, `generation`,
`batch_id`, `env`. An allowlist rather than "keep what arrived": a
dimension anyone can invent is a cardinality incident waiting to happen.
Adding one is a config change, not a release.

Three caller spellings fold onto a canonical name, so one filter works
whichever client sent the request:

| Sent as | Stored as |
|---|---|
| `event_processing_tag` | `stage` |
| `generation_name` | `generation` |
| `trace_user_id` | `org_id` |

`metadata.tags`, `metadata.session_id` and `metadata.trace_name` are
read but not stored: they restate values already captured, and one fact
in two spellings makes a log wider without making it more answerable.
Non-scalar values under an allowed key are skipped, values are capped at
`usage.trace_value_chars` (128), and a body that is not JSON yields no
dimensions rather than an error — attribution is a side channel and
never fails a request.

### `metadata` is consumed, not forwarded

The gateway strips the top-level `metadata` member before sending a
request upstream. This is not tidiness — the field name is taken on
every provider and none of them accept the shape callers send here:
OpenAI allows at most sixteen string→string pairs, Anthropic allows
`metadata.user_id` and nothing else. Forwarding a caller's nested
`trace_metadata` earns a `400`, not a shrug. It is also what the
LiteLLM proxy this replaces already did, so callers see no change.

Removal is span-based on the same-dialect path: the scanner that already
locates `model` records where `metadata` sits, and both edits land in one
copy. No extra parse, no re-serialize, no measurable hot-path cost.

The one consequence worth knowing: a caller relying on **OpenAI's own**
`metadata` passthrough (16 string pairs, stored on OpenAI's side) will
not get it through this gateway. `/passthrough/{provider}/…` is
unaffected — it is an opaque escape hatch and forwards bodies untouched.

Filter with `meta.<key>=<value>` on `/requests`, `/requests/summary`
and `/usage/summary`. Terms are conjunctive:

```
GET /admin/api/requests?meta.workflow_id=WORKFLOW_HCC_CONFIRMED&meta.stage=ICD_EXTRACTION
```

A term naming a dimension no record carries matches nothing rather than
being ignored.

Hourly rollups are keyed by provider, model and key alone, so they
cannot answer a caller dimension. Two consequences follow. The summary
endpoints, which normally read rollups, fall back to scanning records
whenever a `meta.*` term is present — correct, and slower. And
`/history` does not accept `meta.*` at all: it is served from rollups
with no record path, so honouring the filter is impossible and ignoring
it would report totals for traffic the caller did not ask about.

### Also on every record

Four things the gateway already knew and used to discard:

| Field | Why it is there |
|---|---|
| `error_class` | `429` is both "slow down" (`rate_limited`) and "out of quota" (`insufficient_quota`); the status cannot tell you which |
| `seat` | `provider/key` — which *account* served it, which `provider` alone cannot say under an account pool |
| `ttft_ms` | Time to first byte; on a long generation `latency_ms` describes the tail, not the wait |
| `queue_lag_ms` | From the caller's `event_create_ts` — backlog *in front of* the gateway, which no gateway-side timer can see |

## The receipt headers

Client-visible observability on every response:

| Header | Meaning |
|---|---|
| `x-request-id` | correlation id |
| `x-rapid-provider`, `x-rapid-model` | who actually served (fallbacks disclosed) |
| `x-rapid-attempts` | candidates tried |
| `x-rapid-overhead-us` | this request's measured gateway overhead — the promise, auditable per request |
