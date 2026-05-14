//! Streaming-body helper.
//!
//! Adapts any `Stream<Item = Result<Bytes, Error>>` into the crate's
//! [`ResponseBody`] alias. Each item the stream yields becomes one
//! [`http_body::Frame::data`] → one chunked-transfer chunk on the
//! wire → one TCP flush.
//!
//! Slow consumers naturally backpressure: hyper stops polling
//! `Body::poll_frame` until the previous chunk drains, which stops
//! the stream from being polled.

use bytes::Bytes;
use futures_core::Stream;
use http_body::Frame;
use http_body_util::{BodyExt, StreamBody};

use crate::body::ResponseBody;

/// Wrap a `Stream` as a streaming HTTP body.
///
/// Used by handlers that want to emit a response body progressively:
/// chunked logs, audit dumps, future SSE non-event payloads. SSE
/// itself uses [`crate::sse::response`] which provides the right
/// headers in one call.
pub fn stream<S>(s: S) -> ResponseBody
where
    S: Stream<Item = Result<Bytes, crate::Error>> + Send + Sync + 'static,
{
    // Adapt `Stream<Item = Result<Bytes, Error>>` →
    // `Stream<Item = Result<Frame<Bytes>, Error>>`.
    struct Framed<S>(S);

    impl<S> Stream for Framed<S>
    where
        S: Stream<Item = Result<Bytes, crate::Error>> + Unpin,
    {
        type Item = Result<Frame<Bytes>, crate::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::pin::Pin::new(&mut self.0)
                .poll_next(cx)
                .map(|opt| opt.map(|res| res.map(Frame::data)))
        }
    }

    // Box-pin the input so the adapter is Unpin (the upstream
    // `S: Stream` need not be). `Sync` is required by `BoxBody`'s
    // bound chain.
    let pinned: std::pin::Pin<Box<dyn Stream<Item = Result<Bytes, crate::Error>> + Send + Sync>> =
        Box::pin(s);
    StreamBody::new(Framed(pinned)).boxed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use futures_core::Stream;
    use http_body_util::BodyExt;
    use std::pin::Pin;
    use std::task::{Context, Poll};

    /// Simple Vec-backed stream for tests.
    struct VecStream(std::collections::VecDeque<Result<Bytes, crate::Error>>);

    impl Stream for VecStream {
        type Item = Result<Bytes, crate::Error>;
        fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            Poll::Ready(self.0.pop_front())
        }
    }

    #[tokio::test]
    async fn three_chunks_concatenate() {
        let v = VecStream(
            vec![
                Ok(Bytes::from_static(b"hello ")),
                Ok(Bytes::from_static(b"brain ")),
                Ok(Bytes::from_static(b"http")),
            ]
            .into(),
        );
        let body = stream(v);
        let collected = body.collect().await.expect("collect").to_bytes();
        assert_eq!(collected.as_ref(), b"hello brain http");
    }

    #[tokio::test]
    async fn empty_stream_yields_empty_body() {
        let v = VecStream(Default::default());
        let body = stream(v);
        let collected = body.collect().await.expect("collect").to_bytes();
        assert!(collected.is_empty());
    }
}
