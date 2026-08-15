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

    s.ws();
    s.eat(b'{')?;
    s.ws();
    if s.peek() == Some(b'}') {
        return None; // no model field
    }
    loop {
        s.ws();
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
    })
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
    fn string_token(&mut self) -> Option<(usize, usize)> {
        let start = self.i;
        self.eat(b'"')?;
        loop {
            match self.next()? {
                b'"' => return Some((start, self.i)),
                b'\\' => {
                    self.next()?;
                }
                _ => {}
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
}
