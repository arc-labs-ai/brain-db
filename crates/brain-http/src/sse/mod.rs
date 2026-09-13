//! Server-Sent Events (server side).
//!
//! Wire format per WHATWG HTML §9.2. Server-side only for now; a reconnecting
//! `EventSource` client is a follow-up paired with the HTTP-client
//! decision. Server-side reconnect works through the standard
//! `Request<_>::headers()` API: the handler reads `Last-Event-ID`
//! itself and resumes from the right position.
//!
//! ## Pattern
//!
//! ```ignore
//! use brain_http::sse::{self, SseEvent};
//!
//! async fn handler(
//!     req: http::Request<hyper::body::Incoming>,
//! ) -> brain_http::Result<http::Response<brain_http::body::ResponseBody>> {
//!     // Read `Last-Event-ID` for resume.
//!     let resume_from = req
//!         .headers()
//!         .get("last-event-id")
//!         .and_then(|v| v.to_str().ok())
//!         .and_then(|s| s.parse::<u64>().ok())
//!         .unwrap_or(0);
//!
//!     // Typical pattern: bounded mpsc + ReceiverStream.
//!     let (tx, rx) = tokio::sync::mpsc::channel::<SseEvent>(32);
//!     tokio::spawn(async move {
//!         for i in (resume_from + 1).. {
//!             if tx.send(SseEvent::new().with_id(i.to_string())).await.is_err() {
//!                 return; // consumer disconnected
//!             }
//!         }
//!     });
//!     let events = tokio_stream::wrappers::ReceiverStream::new(rx);
//!     Ok(sse::response(events))
//! }
//! ```
//!
//! ## Backpressure
//!
//! Producers should feed events through a **bounded** channel
//! (`tokio::sync::mpsc::channel(N)` then
//! `tokio_stream::wrappers::ReceiverStream`). When the SSE consumer
//! is slow, hyper stops polling `Body::poll_frame`, which stops
//! polling the inner `Stream`, which fills the producer's channel
//! and applies backpressure. **Brain-http does not enforce this** —
//! it's the application's job.
//!
//! ## Flush discipline
//!
//! `SseStream::poll_frame` yields exactly one event per frame.
//! Each frame becomes its own chunked-transfer chunk and gets
//! flushed. A common bug pattern — the framework buffering multiple
//! events into one chunk — is structurally impossible here.

mod encoder;
mod event;
mod stream;

pub use encoder::encode;
pub use event::SseEvent;
pub use stream::SseStream;

use futures_core::Stream;
use http::{Response, StatusCode};
use http_body_util::BodyExt;

use crate::body::ResponseBody;

/// Build a `Response<ResponseBody>` ready to return from a handler.
///
/// Sets the spec'd headers:
/// - `Content-Type: text/event-stream`
/// - `Cache-Control: no-cache`
/// - `X-Accel-Buffering: no` (nginx hint — prevents the reverse
///   proxy from buffering chunks).
///
/// Transfer-encoding is chunked, applied automatically by hyper
/// because the body has no `Content-Length`.
///
/// Returns a `Response`; the handler wraps it in `Ok(...)`.
#[must_use]
pub fn response<S>(events: S) -> Response<ResponseBody>
where
    S: Stream<Item = SseEvent> + Send + Sync + 'static,
{
    let body = SseStream::new(events).boxed();
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .header("cache-control", "no-cache")
        .header("x-accel-buffering", "no")
        .body(body)
        .expect("static SSE response always builds")
}
