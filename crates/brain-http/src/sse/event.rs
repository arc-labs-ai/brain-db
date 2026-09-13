//! `SseEvent` — one Server-Sent Event.
//!
//! Wire format per WHATWG HTML §9.2.6. Fields map 1:1 to the wire shape:
//!
//! ```text
//! id: <id>\n
//! event: <event>\n
//! data: <data>\n
//! retry: <millis>\n
//! \n
//! ```

use std::time::Duration;

/// One SSE event. Construct via `new()` or `Default`, then chain
/// `with_*` setters.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SseEvent {
    /// `id:` field. Clients reflect this back in the
    /// `Last-Event-ID` request header on reconnect — see
    /// [`crate::sse`] module docs for the resume pattern.
    pub id: Option<String>,

    /// `event:` field — custom event name. Absent ⟹ default
    /// `"message"` event on the client side.
    pub event: Option<String>,

    /// `data:` field. May contain newlines; the encoder splits each
    /// line into its own `data:` line per the spec.
    pub data: String,

    /// `retry:` field — reconnect delay in milliseconds. Clients
    /// use this as their next-reconnect timer.
    pub retry: Option<Duration>,
}

impl SseEvent {
    /// New empty event. Fluent setters via the `with_*` methods.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set the `id` field.
    #[must_use]
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = Some(id.into());
        self
    }

    /// Set the `event` field.
    #[must_use]
    pub fn with_event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }

    /// Set the `data` field. Multi-line input is supported; the
    /// encoder emits one `data:` line per source line.
    #[must_use]
    pub fn with_data(mut self, data: impl Into<String>) -> Self {
        self.data = data.into();
        self
    }

    /// Set the `retry` reconnect delay.
    #[must_use]
    pub fn with_retry(mut self, retry: Duration) -> Self {
        self.retry = Some(retry);
        self
    }
}
