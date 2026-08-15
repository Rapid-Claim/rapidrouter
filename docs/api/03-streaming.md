# Streaming

Most chat traffic sets `"stream": true`, so streaming is the primary path,
engineered directly — not a wrapper around the buffered path.

## Wire formats bridged

| Dialect / provider | Stream format |
|---|---|
| OpenAI & compatibles | SSE: `data: {chunk}` … `data: [DONE]` |
| Anthropic | SSE with named events: `message_start`, `content_block_delta`, `message_delta`, `message_stop`, `ping` |
| Gemini | SSE (`alt=sse`) of `GenerateContentResponse` chunks |
| Bedrock | AWS event-stream binary framing (not SSE) |

## The pipeline

```
upstream body stream (Bytes chunks, arbitrary boundaries)
        │
        ▼
  SseCodec / EventStreamCodec   — reassembles frames across chunk splits;
        │                          operates on Bytes, allocates nothing per event
        ▼
  adapter stream translator     — provider event → inbound-dialect event
        │                          (small state machine per stream)
        ▼
  client SSE writer             — framing, flush per event, TCP_NODELAY
```

- **Flush per event, always.** Time-to-first-token is the user-visible
  number; nothing buffers more than one event.
- **Same-dialect fast path**: raw frame forwarding — zero JSON per chunk.
  Frame boundaries are still watched to detect terminal events and to
  accumulate usage for metering.
- **Translation state machines** carry the per-stream context each dialect
  omits from individual events: content-block index → tool-call index
  mapping, role announcement, finish-reason mapping, usage accumulation.
  Budget: ~0.5 µs per translated chunk.
- **Keep-alives**: upstream ping events are absorbed; the gateway emits SSE
  comments on a timer (`stream_idle_ping`, default 15 s) if the upstream
  goes quiet, so intermediaries don't kill the connection.
- **Usage reporting**: `stream_options.include_usage` honored on `/v1`;
  usage is translated from each provider's reporting into the dialect's
  final-usage convention, and always captured for metering regardless.

## Failure and cancellation semantics

- **Before the first byte** reaches the client, normal retry/fallback rules
  apply — the client never knows a fallback happened except via the
  `x-caret-provider` header.
- **After the first byte**, the gateway never fails over mid-stream (the
  conversation state would be corrupt); errors surface as the dialect's
  terminal error event, then the stream closes cleanly.
- **Client disconnect** cancels the upstream request immediately — drop
  semantics abort the hyper body, freeing provider capacity and the
  concurrency permit within microseconds.

## Metrics

Per-stream: TTFT (request-in → first content event out), per-chunk gateway
overhead (event-in → event-out), chunk count, total duration. TTFT overhead
is the advertised number and is tracked by the streaming rig in CI
([../operations/benchmarking.md](../operations/benchmarking.md)).
