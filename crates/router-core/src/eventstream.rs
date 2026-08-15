//! The AWS event-stream binary framing (`application/vnd.amazon.eventstream`),
//! as used by streaming Bedrock responses. Incremental decode over raw
//! network chunks, plus an encoder for the test mock.
//!
//! Frame layout: `[total_len u32][headers_len u32][prelude_crc u32]`
//! `[headers][payload][message_crc u32]`, big-endian. Headers are
//! `name_len u8, name, type u8, value…`; the only type used here is 7
//! (string: `len u16, bytes`). Decoded frames surface as [`SseEvent`]s —
//! the `:event-type` header becomes the event name, the payload the data —
//! so stream translators consume one shape regardless of wire framing.

use bytes::{BufMut, BytesMut};

use crate::sse::SseEvent;

const PRELUDE_LEN: usize = 12;

#[derive(Debug, Default)]
pub struct EventStreamParser {
    buffer: BytesMut,
}

impl EventStreamParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a network chunk; returns every complete frame it finishes.
    /// Frames with corrupt CRCs are dropped (the stream continues at the
    /// declared frame boundary).
    pub fn push(&mut self, chunk: &[u8]) -> Vec<SseEvent> {
        self.buffer.extend_from_slice(chunk);
        let mut events = Vec::new();
        loop {
            if self.buffer.len() < PRELUDE_LEN {
                return events;
            }
            let total_len = u32::from_be_bytes(self.buffer[0..4].try_into().unwrap()) as usize;
            if !(PRELUDE_LEN + 4..=16 * 1024 * 1024).contains(&total_len) {
                // Unrecoverable framing corruption: drop the buffer.
                self.buffer.clear();
                return events;
            }
            if self.buffer.len() < total_len {
                return events;
            }
            let frame = self.buffer.split_to(total_len);
            if let Some(event) = decode_frame(&frame) {
                events.push(event);
            }
        }
    }

    pub fn pending(&self) -> usize {
        self.buffer.len()
    }
}

fn decode_frame(frame: &[u8]) -> Option<SseEvent> {
    let headers_len = u32::from_be_bytes(frame[4..8].try_into().unwrap()) as usize;
    let declared_prelude_crc = u32::from_be_bytes(frame[8..12].try_into().unwrap());
    if crc32fast::hash(&frame[0..8]) != declared_prelude_crc {
        return None;
    }
    let payload_end = frame.len().checked_sub(4)?;
    let headers_end = PRELUDE_LEN.checked_add(headers_len)?;
    if headers_end > payload_end {
        return None;
    }
    let declared_message_crc = u32::from_be_bytes(frame[payload_end..].try_into().ok()?);
    if crc32fast::hash(&frame[..payload_end]) != declared_message_crc {
        return None;
    }

    let mut event_type = None;
    let mut message_type = None;
    let mut headers = &frame[PRELUDE_LEN..headers_end];
    while !headers.is_empty() {
        let name_len = headers[0] as usize;
        let name = std::str::from_utf8(headers.get(1..1 + name_len)?).ok()?;
        let value_kind = *headers.get(1 + name_len)?;
        let mut cursor = 1 + name_len + 1;
        let value = match value_kind {
            7 => {
                let len =
                    u16::from_be_bytes(headers.get(cursor..cursor + 2)?.try_into().ok()?) as usize;
                cursor += 2;
                let v = std::str::from_utf8(headers.get(cursor..cursor + len)?).ok()?;
                cursor += len;
                Some(v.to_owned())
            }
            _ => return None, // only string headers appear in this protocol
        };
        match name {
            ":event-type" => event_type = value,
            ":message-type" => message_type = value,
            _ => {}
        }
        headers = &headers[cursor..];
    }

    if message_type.as_deref() == Some("exception") {
        // Exceptions carry the error type in :exception-type; surface as
        // an `exception` event for the translator to map.
        event_type = Some("exception".to_owned());
    }

    let payload = String::from_utf8_lossy(&frame[headers_end..payload_end]).into_owned();
    Some(SseEvent {
        event: event_type,
        data: payload,
    })
}

/// Encode one event frame (mock/test side).
pub fn encode_event(event_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut headers = Vec::new();
    for (name, value) in [
        (":event-type", event_type),
        (":message-type", "event"),
        (":content-type", "application/json"),
    ] {
        headers.push(name.len() as u8);
        headers.extend_from_slice(name.as_bytes());
        headers.push(7u8);
        headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
        headers.extend_from_slice(value.as_bytes());
    }

    let total_len = PRELUDE_LEN + headers.len() + payload.len() + 4;
    let mut out = BytesMut::with_capacity(total_len);
    out.put_u32(total_len as u32);
    out.put_u32(headers.len() as u32);
    out.put_u32(crc32fast::hash(&out[0..8]));
    out.extend_from_slice(&headers);
    out.extend_from_slice(payload);
    let message_crc = crc32fast::hash(&out[..]);
    out.put_u32(message_crc);
    out.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_frames() {
        let frame = encode_event("contentBlockDelta", br#"{"delta":{"text":"hi"}}"#);
        let mut parser = EventStreamParser::new();
        let events = parser.push(&frame);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("contentBlockDelta"));
        assert_eq!(events[0].data, r#"{"delta":{"text":"hi"}}"#);
        assert_eq!(parser.pending(), 0);
    }

    #[test]
    fn split_frames_reassemble_at_any_boundary() {
        let mut stream = Vec::new();
        stream.extend(encode_event("messageStart", br#"{"role":"assistant"}"#));
        stream.extend(encode_event("messageStop", br#"{"stopReason":"end_turn"}"#));

        for cut in 1..stream.len() {
            let mut parser = EventStreamParser::new();
            let mut events = parser.push(&stream[..cut]);
            events.extend(parser.push(&stream[cut..]));
            assert_eq!(events.len(), 2, "cut at {cut}");
            assert_eq!(
                events[0].event.as_deref(),
                Some("messageStart"),
                "cut at {cut}"
            );
            assert_eq!(
                events[1].event.as_deref(),
                Some("messageStop"),
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn corrupt_crc_drops_frame_not_stream() {
        let mut bad = encode_event("messageStart", b"{}");
        let idx = bad.len() - 6;
        bad[idx] ^= 0xff; // corrupt payload after CRCs were computed
        let good = encode_event("messageStop", b"{}");

        let mut parser = EventStreamParser::new();
        let mut events = parser.push(&bad);
        events.extend(parser.push(&good));
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event.as_deref(), Some("messageStop"));
    }

    #[test]
    fn garbage_never_panics() {
        fastrand::seed(3);
        for _ in 0..200 {
            let junk: Vec<u8> = (0..fastrand::usize(..200))
                .map(|_| fastrand::u8(..))
                .collect();
            let mut parser = EventStreamParser::new();
            let _ = parser.push(&junk);
        }
    }

    #[test]
    fn exception_frames_surface_as_exception_events() {
        let mut frame_headers = Vec::new();
        for (name, value) in [
            (":exception-type", "throttlingException"),
            (":message-type", "exception"),
        ] {
            frame_headers.push(name.len() as u8);
            frame_headers.extend_from_slice(name.as_bytes());
            frame_headers.push(7u8);
            frame_headers.extend_from_slice(&(value.len() as u16).to_be_bytes());
            frame_headers.extend_from_slice(value.as_bytes());
        }
        let payload = br#"{"message":"slow down"}"#;
        let total_len = 12 + frame_headers.len() + payload.len() + 4;
        let mut out = bytes::BytesMut::new();
        use bytes::BufMut;
        out.put_u32(total_len as u32);
        out.put_u32(frame_headers.len() as u32);
        out.put_u32(crc32fast::hash(&out[0..8]));
        out.extend_from_slice(&frame_headers);
        out.extend_from_slice(payload);
        let crc = crc32fast::hash(&out[..]);
        out.put_u32(crc);

        let mut parser = EventStreamParser::new();
        let events = parser.push(&out);
        assert_eq!(events[0].event.as_deref(), Some("exception"));
    }
}
