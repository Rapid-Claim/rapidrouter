//! An incremental SSE codec over raw bytes.
//!
//! Network chunks split anywhere — mid-line, mid-`data:` prefix, inside a
//! UTF-8 sequence — so the parser buffers bytes and only materializes an
//! event when its terminating blank line has fully arrived. Multi-line
//! `data:` fields, `event:` names, comments, and CRLF line endings all
//! follow the SSE spec.

use bytes::BytesMut;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SseEvent {
    /// The `event:` field, if any.
    pub event: Option<String>,
    /// All `data:` lines joined with `\n`, per spec.
    pub data: String,
}

#[derive(Debug, Default)]
pub struct SseParser {
    buffer: BytesMut,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a network chunk; returns every event completed by it.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        while let Some(event) = self.next_event() {
            events.push(event);
        }
        events
    }

    /// Bytes buffered but not yet forming a complete event (diagnostics /
    /// end-of-stream truncation detection).
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    fn next_event(&mut self) -> Option<SseEvent> {
        let boundary = find_event_boundary(&self.buffer)?;
        let raw = self.buffer.split_to(boundary.end);
        let block = &raw[..boundary.start];

        let mut event_name: Option<String> = None;
        let mut data_lines: Vec<&str> = Vec::new();
        for line in split_lines(block) {
            let Ok(line) = std::str::from_utf8(line) else {
                continue; // invalid UTF-8 in a field line: drop the line, keep the stream
            };
            if line.starts_with(':') {
                continue; // comment / keep-alive
            }
            let (field, value) = match line.split_once(':') {
                Some((f, v)) => (f, v.strip_prefix(' ').unwrap_or(v)),
                None => (line, ""),
            };
            match field {
                "data" => data_lines.push(value),
                "event" => event_name = Some(value.to_owned()),
                _ => {} // id, retry, unknown fields: not our concern
            }
        }

        if data_lines.is_empty() && event_name.is_none() {
            // A block of only comments; emit nothing and continue.
            return self.next_event();
        }
        Some(SseEvent {
            event: event_name,
            data: data_lines.join("\n"),
        })
    }
}

struct Boundary {
    /// End of the event's content.
    start: usize,
    /// First byte after the blank-line terminator.
    end: usize,
}

/// Find the first blank-line terminator (`\n\n`, `\r\n\r\n`, or mixed).
fn find_event_boundary(buf: &[u8]) -> Option<Boundary> {
    let mut i = 0;
    while i < buf.len() {
        // A line break at i…
        let first = line_break_len(&buf[i..])?;
        if first == 0 {
            i += 1;
            continue;
        }
        // …immediately followed by another is the terminator, unless the
        // second break is still incomplete (`\r` at end of buffer).
        match line_break_len(&buf[i + first..]) {
            Some(0) => i += first,
            Some(second) => {
                return Some(Boundary {
                    start: i,
                    end: i + first + second,
                });
            }
            None => return None,
        }
    }
    None
}

/// Length of the line break starting exactly at `buf[0]`: 1 for `\n`,
/// 2 for `\r\n`, 0 for a non-break byte, `None` when a lone trailing `\r`
/// needs more bytes to classify.
fn line_break_len(buf: &[u8]) -> Option<usize> {
    match buf.first() {
        Some(b'\n') => Some(1),
        Some(b'\r') => match buf.get(1) {
            Some(b'\n') => Some(2),
            Some(_) => Some(1), // lone \r is a break per spec
            None => None,
        },
        Some(_) => Some(0),
        None => Some(0),
    }
}

fn split_lines(block: &[u8]) -> impl Iterator<Item = &[u8]> {
    block
        .split(|&b| b == b'\n')
        .map(|line| line.strip_suffix(b"\r").unwrap_or(line))
        .filter(|line| !line.is_empty())
}

/// Format an event for the wire.
pub fn format_event(event: Option<&str>, data: &str) -> String {
    match event {
        Some(name) => format!("event: {name}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(parser: &mut SseParser, bytes: &[u8]) -> Vec<SseEvent> {
        parser.push(bytes)
    }

    #[test]
    fn whole_events_parse() {
        let mut p = SseParser::new();
        let events = one(&mut p, b"data: {\"a\":1}\n\ndata: [DONE]\n\n");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].data, "{\"a\":1}");
        assert_eq!(events[1].data, "[DONE]");
    }

    #[test]
    fn named_events_and_crlf() {
        let mut p = SseParser::new();
        let events = one(&mut p, b"event: message_start\r\ndata: {}\r\n\r\n");
        assert_eq!(events[0].event.as_deref(), Some("message_start"));
        assert_eq!(events[0].data, "{}");
    }

    #[test]
    fn split_mid_prefix_and_mid_payload() {
        let mut p = SseParser::new();
        assert!(one(&mut p, b"da").is_empty());
        assert!(one(&mut p, b"ta: {\"x\":").is_empty());
        assert!(one(&mut p, b" 1}").is_empty());
        let events = one(&mut p, b"\n\n");
        assert_eq!(events[0].data, "{\"x\": 1}");
    }

    #[test]
    fn split_mid_utf8_sequence() {
        let text = "data: {\"t\":\"héllo → wörld\"}\n\n".as_bytes();
        // Split at every byte position; each split must yield exactly the
        // same single event.
        for cut in 1..text.len() {
            let mut p = SseParser::new();
            let mut events = p.push(&text[..cut]);
            events.extend(p.push(&text[cut..]));
            assert_eq!(events.len(), 1, "cut at {cut}");
            assert_eq!(events[0].data, "{\"t\":\"héllo → wörld\"}", "cut at {cut}");
        }
    }

    #[test]
    fn multiline_data_joins_with_newline() {
        let mut p = SseParser::new();
        let events = one(&mut p, b"data: line1\ndata: line2\n\n");
        assert_eq!(events[0].data, "line1\nline2");
    }

    #[test]
    fn comments_and_pings_are_swallowed() {
        let mut p = SseParser::new();
        assert!(one(&mut p, b": keep-alive\n\n").is_empty());
        let events = one(&mut p, b": ping\ndata: real\n\n");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].data, "real");
    }

    #[test]
    fn crlf_terminator_split_across_chunks() {
        let mut p = SseParser::new();
        assert!(one(&mut p, b"data: x\r\n\r").is_empty());
        let events = one(&mut p, b"\n");
        assert_eq!(events[0].data, "x");
    }

    #[test]
    fn field_without_space_after_colon() {
        let mut p = SseParser::new();
        let events = one(&mut p, b"data:tight\n\n");
        assert_eq!(events[0].data, "tight");
    }

    #[test]
    fn pending_reports_truncation() {
        let mut p = SseParser::new();
        one(&mut p, b"data: incomplete");
        assert!(p.pending() > 0);
    }

    #[test]
    fn arbitrary_chunking_is_boundary_invariant() {
        let stream =
            b"event: a\ndata: 1\n\n: ping\n\ndata: 2\ndata: 3\r\n\r\nevent: b\r\ndata: [DONE]\n\n";
        let expected = {
            let mut p = SseParser::new();
            p.push(stream)
        };
        assert_eq!(expected.len(), 3);

        fastrand::seed(11);
        for _ in 0..200 {
            let mut p = SseParser::new();
            let mut got = Vec::new();
            let mut rest: &[u8] = stream;
            while !rest.is_empty() {
                let take = 1 + fastrand::usize(..rest.len());
                got.extend(p.push(&rest[..take]));
                rest = &rest[take..];
            }
            assert_eq!(got, expected);
        }
    }
}
