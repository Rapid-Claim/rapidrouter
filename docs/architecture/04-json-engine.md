# The JSON Engine

JSON handling is where gateway latency lives or dies, so rapid-router treats
it as a tiered engine: the common case does the least possible work, and
every shortcut is fuzz-proven equivalent to the straightforward path.

## Tier 1 — Don't parse: the splice path

When the inbound and outbound dialects match (OpenAI-shaped request to an
OpenAI-compatible provider), the body needs exactly one change: the `model`
value (strip the `provider/` prefix or resolve an alias).

1. sonic-rs locates the `"model"` value span with a SIMD structural scan —
   no DOM, no allocation.
2. The buffer is spliced as a rope: `[..start] ⊕ new_model ⊕ [end..]` —
   the untouched regions (the messages array: 95 %+ of the body) are
   forwarded as refcounted slices, never copied, never fully validated
   beyond the structural scan.
3. Auth headers are set; the request goes upstream.

Cost: 1–3 µs for a typical 2 KB body, near-flat in body size because SIMD
scanning runs at GB/s. Anything surprising at the splice site (non-string
`model`, escapes) falls back to Tier 2 — the fast path always has a safe
exit.

## Tier 2 — Typed, borrowed, single-pass: the translation path

Cross-dialect translation deserializes into borrow-friendly types and writes
the target dialect in the same pass — never source → tree → target.

```rust
#[derive(Deserialize)]
pub struct ChatRequest<'a> {
    #[serde(borrow)] pub model: &'a str,
    #[serde(borrow)] pub messages: Vec<Message<'a>>,   // content: Cow<'a, str>
    pub temperature: Option<f32>,
    pub stream: Option<bool>,
    #[serde(borrow, flatten)] pub rest: PassthroughMap<'a>,  // &RawValue leaves
}
```

- String content borrows from the request buffer; `Cow` goes owned only
  when JSON escapes force it.
- Base64 media blobs (images, audio, files) are treated as **opaque byte
  spans** — moved by reference into the output, never decoded.
- Unknown fields are `&RawValue`: re-emitted verbatim, or dropped loudly
  (metric + debug log) when the target dialect can't carry them.
- Deserializer: sonic-rs's serde front end. Serialization targets one
  pre-sized `BytesMut`.

## Tier 3 — serde_json: cold paths

Config files, `/v1/models`, error bodies, tests, tooling. Boring and
correct where nothing is hot.

## Streaming chunks

Stream deltas are small and frequent — per-event overhead multiplies by
chunk count, so:

- Same-dialect streams forward **raw frames**: zero JSON work per chunk.
  Frame boundaries are still tracked (to detect terminal events and
  accumulate usage) by the SSE codec, which operates on `Bytes` and
  allocates nothing per event.
- Translated streams parse each provider event into a small borrowed struct
  and emit the target chunk through a hand-written writer — template
  prefix/suffix with escaped-content splice. Budget: 0.5 µs per chunk.

## The rules

1. Never `String` where `&[u8]`/`Bytes` serves.
2. Never a DOM (`Value` tree) on the hot path — trees are for tests.
3. All spliced content passes through the escape-aware writer.
4. **Fuzzing is not optional.** cargo-fuzz targets prove: splice ≡
   full-parse rewrite; translator round-trips preserve semantics; the SSE
   codec never mis-frames on adversarial chunk boundaries.
5. Criterion benches per tier gate CI at a 10 % regression threshold.
