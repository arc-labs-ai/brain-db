//! Encode [`SseEvent`] → wire `Bytes`.
//!
//! Wire format per WHATWG HTML §9.2.6:
//!
//! ```text
//! id: <id>\n
//! event: <event>\n
//! data: <line1>\n
//! data: <line2>\n
//! retry: <millis>\n
//! \n
//! ```
//!
//! Multi-line `data` is split into one `data:` line per source line
//! (lines separated by `\n`; the encoder normalises `\r\n` and `\r`
//! to `\n` on the way in). The empty trailing newline ("dispatch
//! event" trigger) terminates one event.

use std::fmt::Write as _;

use bytes::{BufMut, Bytes, BytesMut};

use crate::sse::event::SseEvent;

const NEWLINE: &[u8] = b"\n";

/// Encode one event into a contiguous `Bytes` chunk.
///
/// The output is suitable for wrapping in a single
/// [`http_body::Frame::data`] — that's exactly what
/// [`crate::sse::SseStream`] does to enforce one-event-per-frame
/// flush discipline.
#[must_use]
pub fn encode(event: &SseEvent) -> Bytes {
    // Pre-size with a reasonable guess: keys + data + dispatch newline.
    let est = event.id.as_ref().map_or(0, |s| s.len() + 5)
        + event.event.as_ref().map_or(0, |s| s.len() + 8)
        + event.data.len()
        + 32
        + 2;
    let mut buf = BytesMut::with_capacity(est);

    if let Some(id) = &event.id {
        buf.put_slice(b"id: ");
        buf.put_slice(id.as_bytes());
        buf.put_slice(NEWLINE);
    }
    if let Some(name) = &event.event {
        buf.put_slice(b"event: ");
        buf.put_slice(name.as_bytes());
        buf.put_slice(NEWLINE);
    }
    // Normalise CRLF / CR → LF on the way in so the per-line split
    // produces the spec-correct number of `data:` lines.
    let normalised = normalise_lines(&event.data);
    for line in normalised.split('\n') {
        buf.put_slice(b"data: ");
        buf.put_slice(line.as_bytes());
        buf.put_slice(NEWLINE);
    }
    if let Some(retry) = event.retry {
        buf.put_slice(b"retry: ");
        let mut tmp = String::with_capacity(20);
        write!(&mut tmp, "{}", retry.as_millis()).expect("invariant: write to String is infallible");
        buf.put_slice(tmp.as_bytes());
        buf.put_slice(NEWLINE);
    }
    // Empty line: end-of-event marker.
    buf.put_slice(NEWLINE);
    buf.freeze()
}

/// Normalise line endings to `\n`. Cheap when input already uses LF
/// (no allocation); falls back to a small allocation when CR is
/// present.
fn normalise_lines(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\r') {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn minimal_event_only_terminator() {
        let e = SseEvent::new();
        let b = encode(&e);
        assert_eq!(b.as_ref(), b"data: \n\n");
    }

    #[test]
    fn id_and_data() {
        let e = SseEvent::new().with_id("42").with_data("hello");
        let b = encode(&e);
        assert_eq!(b.as_ref(), b"id: 42\ndata: hello\n\n");
    }

    #[test]
    fn full_fields_ordered() {
        let e = SseEvent::new()
            .with_id("1")
            .with_event("update")
            .with_data("payload")
            .with_retry(Duration::from_millis(5000));
        let b = encode(&e);
        assert_eq!(
            std::str::from_utf8(&b).unwrap(),
            "id: 1\nevent: update\ndata: payload\nretry: 5000\n\n"
        );
    }

    #[test]
    fn multi_line_data_splits() {
        let e = SseEvent::new().with_data("line1\nline2\nline3");
        let b = encode(&e);
        assert_eq!(
            std::str::from_utf8(&b).unwrap(),
            "data: line1\ndata: line2\ndata: line3\n\n"
        );
    }

    #[test]
    fn crlf_data_normalises_to_one_data_line_per_logical_line() {
        let e = SseEvent::new().with_data("a\r\nb\r\nc");
        let b = encode(&e);
        assert_eq!(
            std::str::from_utf8(&b).unwrap(),
            "data: a\ndata: b\ndata: c\n\n"
        );
    }

    #[test]
    fn normalises_bare_cr() {
        let e = SseEvent::new().with_data("a\rb");
        let b = encode(&e);
        assert_eq!(std::str::from_utf8(&b).unwrap(), "data: a\ndata: b\n\n");
    }

    #[test]
    fn retry_only_event_emits_empty_data_and_retry_field() {
        let e = SseEvent::new().with_retry(Duration::from_secs(3));
        let b = encode(&e);
        assert_eq!(std::str::from_utf8(&b).unwrap(), "data: \nretry: 3000\n\n");
    }
}
