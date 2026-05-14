//! Adapt a `Stream<Item = SseEvent>` into an HTTP [`Body`].
//!
//! **Critical correctness property:** one event ⟹ one [`Frame`].
//! hyper writes each frame as its own chunked-transfer chunk and
//! flushes the TCP buffer. Batching multiple events into a single
//! `Frame` defeats SSE's near-real-time promise.

use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures_core::Stream;
use http_body::{Body, Frame, SizeHint};

use crate::sse::encoder::encode;
use crate::sse::event::SseEvent;

pin_project_lite::pin_project! {
    /// `Body` implementation backed by a stream of [`SseEvent`].
    pub struct SseStream<S> {
        #[pin]
        inner: S,
    }
}

impl<S> SseStream<S> {
    /// Wrap an event stream as a `Body`.
    pub fn new(inner: S) -> Self {
        Self { inner }
    }
}

impl<S> Body for SseStream<S>
where
    S: Stream<Item = SseEvent> + Send + 'static,
{
    type Data = Bytes;
    type Error = crate::Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Self::Data>, Self::Error>>> {
        let this = self.project();
        match this.inner.poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(ev)) => Poll::Ready(Some(Ok(Frame::data(encode(&ev))))),
        }
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}
