//! Lazy, single-pass extraction of the routing fields (`model`, `stream`)
//! from a JSON request body, and splicing of a replacement `model` value.
//!
//! The same-dialect fast path must not materialize the messages array —
//! typically 95%+ of the body. The scanner walks the top-level object once,
//! skipping every value it does not need. Anything surprising returns
//! `None`, and callers fall back to a full parse: the fast path is an
//! optimization, never a second grammar.

use bytes::{Bytes, BytesMut};

#[derive(Debug, PartialEq)]
pub struct Probe {
    /// Unescaped `model` value.
    pub model: String,
    /// Byte range of the model's string token, including both quotes.
    pub model_span: (usize, usize),
    pub stream: Option<bool>,
    /// Byte range of the whole top-level `metadata` member — its key
    /// token through the end of its value — when the body carries one.
    ///
    /// The gateway *consumes* `metadata`: it is the channel callers
    /// attribute their traffic through, and it is not a field upstreams
    /// should see. Recording the span here means dropping it costs one
    /// copy on a path that was already copying to rewrite the model,
    /// rather than a parse and re-serialize of the whole request.
    pub metadata_span: Option<(usize, usize)>,
}

/// Scan a top-level JSON object for `model` (string) and `stream` (bool).
///
/// Returns `None` when the body is not a well-formed single object, when
/// `model` is missing or not a string, or on any construct the scanner
/// does not handle (escaped keys, non-literal `stream`). Duplicate keys
/// follow last-one-wins, matching serde_json.
pub fn probe(body: &[u8]) -> Option<Probe> {
    let mut s = Scanner { b: body, i: 0 };
    let mut model: Option<(usize, usize)> = None;
    let mut stream: Option<bool> = None;
    let mut metadata: Option<(usize, usize)> = None;

    s.ws();
    s.eat(b'{')?;
    s.ws();
    if s.peek() == Some(b'}') {
        return None; // no model field
    }
    loop {
        s.ws();
        // The member starts at its key token, which is where a removal
        // has to begin.
        let member_start = s.i;
        let (ks, ke) = s.string_token()?;
        let key = &body[ks + 1..ke - 1];
        if key.contains(&b'\\') {
            return None; // escaped key: let the full parser decide
        }
        s.ws();
        s.eat(b':')?;
        s.ws();
        match key {
            b"model" => {
                if s.peek() != Some(b'"') {
                    return None;
                }
                model = Some(s.string_token()?);
            }
            b"stream" => {
                stream = match s.peek() {
                    Some(b't') => {
                        s.literal(b"true")?;
                        Some(true)
                    }
                    Some(b'f') => {
                        s.literal(b"false")?;
                        Some(false)
                    }
                    Some(b'n') => {
                        s.literal(b"null")?;
                        None
                    }
                    _ => return None,
                };
            }
            b"metadata" => {
                if metadata.is_some() {
                    // A duplicate key: last-one-wins would leave the
                    // earlier member in place and forward it upstream,
                    // which is exactly what removing it is meant to
                    // prevent. Hand the body to the full parser, which
                    // collapses duplicates before the member is dropped.
                    return None;
                }
                s.skip_value()?;
                metadata = Some((member_start, s.i));
            }
            _ => s.skip_value()?,
        }
        s.ws();
        match s.next() {
            Some(b',') => continue,
            Some(b'}') => break,
            _ => return None,
        }
    }
    s.ws();
    if s.i != body.len() {
        return None; // trailing content: not a single JSON document
    }

    let (vs, ve) = model?;
    let model_str: String = serde_json::from_slice(&body[vs..ve]).ok()?;
    Some(Probe {
        model: model_str,
        model_span: (vs, ve),
        stream,
        metadata_span: metadata,
    })
}

/// Grow a member's span to swallow the one comma that separated it.
///
/// The preceding comma when there is one, otherwise the following one —
/// and neither for a sole member, which leaves `{}`.
fn member_span_with_separator(body: &[u8], span: (usize, usize)) -> (usize, usize) {
    let (mut start, mut end) = span;
    let is_ws = |c: u8| matches!(c, b' ' | b'\t' | b'\n' | b'\r');
    match body[..start].iter().rposition(|&c| !is_ws(c)) {
        Some(i) if body[i] == b',' => start = i,
        _ => {
            if let Some(offset) = body[end..].iter().position(|&c| !is_ws(c))
                && body[end + offset] == b','
            {
                end += offset + 1;
            }
        }
    }
    (start, end)
}

/// Rewrite the model and drop `metadata` in a single copy.
///
/// Both edits land in one pass, applied in positional order, because
/// each would otherwise invalidate the other's offsets — removing a
/// `metadata` member that sits before `model` shifts the model span left
/// by however much came out.
///
/// The two members are distinct keys of the same object, so one strictly
/// precedes the other and there is no overlap to reconcile.
pub fn splice_model_dropping_metadata(
    body: &Bytes,
    model_span: (usize, usize),
    metadata_span: Option<(usize, usize)>,
    new_model: &str,
) -> Bytes {
    let Some(metadata_span) = metadata_span else {
        return splice_model(body, model_span, new_model);
    };
    let md = member_span_with_separator(body, metadata_span);
    let quoted = serde_json::to_string(new_model).expect("strings always serialize");
    let mut out = BytesMut::with_capacity(body.len() + quoted.len());
    if md.1 <= model_span.0 {
        out.extend_from_slice(&body[..md.0]);
        out.extend_from_slice(&body[md.1..model_span.0]);
        out.extend_from_slice(quoted.as_bytes());
        out.extend_from_slice(&body[model_span.1..]);
    } else {
        out.extend_from_slice(&body[..model_span.0]);
        out.extend_from_slice(quoted.as_bytes());
        out.extend_from_slice(&body[model_span.1..md.0]);
        out.extend_from_slice(&body[md.1..]);
    }
    out.freeze()
}

/// Replace the model token at `span` with `new_model`, JSON-escaped.
/// Everything outside the span is byte-identical to the input.
pub fn splice_model(body: &Bytes, span: (usize, usize), new_model: &str) -> Bytes {
    let quoted = serde_json::to_string(new_model).expect("strings always serialize");
    let mut out = BytesMut::with_capacity(body.len() - (span.1 - span.0) + quoted.len());
    out.extend_from_slice(&body[..span.0]);
    out.extend_from_slice(quoted.as_bytes());
    out.extend_from_slice(&body[span.1..]);
    out.freeze()
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn next(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        Some(c)
    }

    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        (self.next()? == c).then_some(())
    }

    fn literal(&mut self, lit: &[u8]) -> Option<()> {
        if self.b[self.i..].starts_with(lit) {
            self.i += lit.len();
            Some(())
        } else {
            None
        }
    }

    /// Consume a string token; returns its span including both quotes.
    ///
    /// String content dominates request bodies, so this is the scanner's
    /// hot loop: memchr jumps to the next quote or escape instead of
    /// stepping byte-by-byte.
    fn string_token(&mut self) -> Option<(usize, usize)> {
        let start = self.i;
        self.eat(b'"')?;
        loop {
            let offset = memchr::memchr2(b'"', b'\\', &self.b[self.i..])?;
            self.i += offset;
            if self.b[self.i] == b'"' {
                self.i += 1;
                return Some((start, self.i));
            }
            // Escape: skip the backslash and the escaped byte.
            self.i += 2;
            if self.i > self.b.len() {
                return None;
            }
        }
    }

    fn skip_value(&mut self) -> Option<()> {
        match self.peek()? {
            b'"' => self.string_token().map(|_| ()),
            b'{' | b'[' => self.skip_nested(),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            b'-' | b'0'..=b'9' => {
                while matches!(
                    self.peek(),
                    Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
                ) {
                    self.i += 1;
                }
                Some(())
            }
            _ => None,
        }
    }

    /// Skip a balanced object/array, honoring strings (a brace inside a
    /// string does not count).
    fn skip_nested(&mut self) -> Option<()> {
        let mut depth = 0usize;
        loop {
            match self.next()? {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Some(());
                    }
                }
                b'"' => {
                    self.i -= 1;
                    self.string_token()?;
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> Option<Probe> {
        probe(s.as_bytes())
    }

    #[test]
    fn extracts_model_and_stream() {
        let got = p(r#"{"model": "openai/gpt-4o", "stream": true, "messages": []}"#).unwrap();
        assert_eq!(got.model, "openai/gpt-4o");
        assert_eq!(got.stream, Some(true));
    }

    #[test]
    fn model_after_large_messages_array() {
        let got =
            p(r#"{"messages": [{"role":"user","content":"a } tricky \" string"}], "model":"m"}"#)
                .unwrap();
        assert_eq!(got.model, "m");
        assert_eq!(got.stream, None);
    }

    #[test]
    fn nested_model_keys_ignored() {
        let got = p(r#"{"metadata": {"model": "inner"}, "model": "outer"}"#).unwrap();
        assert_eq!(got.model, "outer");
    }

    #[test]
    fn duplicate_model_last_wins() {
        let got = p(r#"{"model": "first", "model": "second"}"#).unwrap();
        assert_eq!(got.model, "second");
    }

    #[test]
    fn escaped_model_value_unescapes() {
        let got = p(r#"{"model": "weird\"name"}"#).unwrap();
        assert_eq!(got.model, "weird\"name");
    }

    #[test]
    fn surprises_fall_back() {
        assert_eq!(p(r#"[1,2]"#), None);
        assert_eq!(p(r#"{"messages": []}"#), None); // no model
        assert_eq!(p(r#"{"model": 42}"#), None); // non-string model
        assert_eq!(p(r#"{"model": "m", "stream": "yes"}"#), None); // non-literal stream
        assert_eq!(p(r#"{"model": "m"} extra"#), None); // trailing content
        assert_eq!(p(r#"{"model": "m""#), None); // truncated
        assert_eq!(p(r#"{"\u006dodel": "m"}"#), None); // escaped key
    }

    #[test]
    fn splice_replaces_only_the_span() {
        let body = Bytes::from_static(br#"{"model": "openai/gpt-4o", "messages": [1,2,3]}"#);
        let got = probe(&body).unwrap();
        let out = splice_model(&body, got.model_span, "gpt-4o");
        assert_eq!(&out[..], br#"{"model": "gpt-4o", "messages": [1,2,3]}"#);
    }

    #[test]
    fn splice_escapes_replacement() {
        let body = Bytes::from_static(br#"{"model":"x"}"#);
        let got = probe(&body).unwrap();
        let out = splice_model(&body, got.model_span, "a\"b");
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "a\"b");
    }

    /// The `metadata` member is found wherever it sits, and its span
    /// covers the key through the end of the value.
    #[test]
    fn probe_locates_the_metadata_member() {
        for body in [
            r#"{"model":"m","metadata":{"a":1}}"#,
            r#"{"metadata":{"a":1},"model":"m"}"#,
            r#"{"model":"m","metadata":{"a":1},"stream":true}"#,
            r#"{ "model" : "m" , "metadata" : { "a" : 1 } }"#,
        ] {
            let probed = probe(body.as_bytes()).expect("scannable");
            let (start, end) = probed.metadata_span.expect("a metadata member");
            assert!(body[start..].starts_with("\"metadata\""), "span {body}");
            assert!(body[..end].ends_with('}'), "span end {body}");
        }
        // Absent when the body carries none.
        assert_eq!(
            probe(br#"{"model":"m","stream":true}"#)
                .unwrap()
                .metadata_span,
            None
        );
    }

    /// Removing a member must leave valid JSON in every position —
    /// alone, first, last, and in the middle — which means the span it
    /// is removed by has to absorb exactly one comma.
    #[test]
    fn a_member_span_absorbs_its_separator() {
        for (body, want) in [
            (r#"{"metadata":{"a":1}}"#, r#"{}"#),
            (r#"{"model":"m","metadata":{"a":1}}"#, r#"{"model":"m"}"#),
            (r#"{"metadata":{"a":1},"model":"m"}"#, r#"{"model":"m"}"#),
            (r#"{"a":1,"metadata":{},"b":2}"#, r#"{"a":1,"b":2}"#),
        ] {
            let span = probe(body.as_bytes())
                .and_then(|p| p.metadata_span)
                .unwrap_or_else(|| find_metadata_span(body));
            let (start, end) = member_span_with_separator(body.as_bytes(), span);
            let out = format!("{}{}", &body[..start], &body[end..]);
            assert_eq!(out, want, "from {body}");
            // The real assertion: what is left still parses.
            let _: serde_json::Value = serde_json::from_str(&out).expect("valid JSON");
        }
    }

    /// Bodies without a `model` are not probeable, so tests that only
    /// care about removal locate the member the simple way.
    fn find_metadata_span(body: &str) -> (usize, usize) {
        let start = body.find("\"metadata\"").expect("a metadata member");
        let value_start = body[start..].find(':').expect("a colon") + start + 1;
        let mut depth = 0usize;
        let bytes = body.as_bytes();
        let mut i = value_start;
        while i < bytes.len() {
            match bytes[i] {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                    if depth == 0 {
                        i += 1;
                        break;
                    }
                }
                b',' if depth == 0 => break,
                _ => {}
            }
            i += 1;
        }
        (start, i)
    }

    /// Both edits in one pass, whichever order the members arrived in —
    /// removing `metadata` shifts every offset after it, so a naive
    /// second edit would splice into the wrong place.
    #[test]
    fn model_and_metadata_are_rewritten_together() {
        for body in [
            r#"{"model":"old/name","metadata":{"trace_metadata":{"workflow_id":"W"}},"stream":true}"#,
            r#"{"metadata":{"trace_metadata":{"workflow_id":"W"}},"model":"old/name","stream":true}"#,
            r#"{"stream":true,"metadata":{},"model":"old/name"}"#,
            r#"{ "model" : "old/name" , "metadata" : { } , "stream" : true }"#,
        ] {
            let bytes = Bytes::copy_from_slice(body.as_bytes());
            let probed = probe(body.as_bytes()).expect("scannable");
            let out = splice_model_dropping_metadata(
                &bytes,
                probed.model_span,
                probed.metadata_span,
                "upstream-model",
            );
            let v: serde_json::Value = serde_json::from_slice(&out).expect("valid JSON");
            assert_eq!(v["model"], "upstream-model", "from {body}");
            assert!(v.get("metadata").is_none(), "metadata survived: {body}");
            assert_eq!(v["stream"], true, "other members disturbed: {body}");
        }
    }

    /// With no metadata to drop, the rewrite is exactly the plain splice.
    #[test]
    fn without_metadata_it_is_the_plain_splice() {
        let body = Bytes::from_static(br#"{"model": "openai/gpt-4o", "messages": [1,2,3]}"#);
        let probed = probe(&body).unwrap();
        let out =
            splice_model_dropping_metadata(&body, probed.model_span, probed.metadata_span, "m");
        assert_eq!(&out[..], br#"{"model": "m", "messages": [1,2,3]}"#);
    }
}
