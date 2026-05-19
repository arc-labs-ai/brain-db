//! `FrameStream<T>` — the generic streaming engine that backs
//! every op's `send_stream()`.
//!
//! Owns the [`PoolGuard`] for its lifetime so back-pressure is
//! demand-driven: each `next().await` reads exactly one frame
//! off the socket. If the caller stops polling, no socket reads
//! happen and TCP backpressure propagates to the server.
//!
//! Retry is intentionally out of scope for streams: SUBSCRIBE's
//! `from_lsn` resume semantics (spec §13/05 §8) require server-
//! side support that isn't wired in v1. Transient errors surface
//! to the caller, who can choose to re-open the stream.

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, ProtocolError};
use futures_lite::stream::Stream;

use crate::error::ClientError;
use crate::ops::common::{map_error_frame, FLAG_EOS};
use crate::pool::PoolGuard;
use crate::proto::frames::read_one_frame;

/// Type-erased decoder: takes a frame's payload, returns the
/// items the frame carries. Returns `Err` on payload-decode
/// failures.
pub type StreamDecoder<T> = Box<dyn FnMut(&[u8]) -> Result<Vec<T>, ClientError> + Send + 'static>;

/// Boxed future that reads one frame from the guarded connection.
/// The future owns the `PoolGuard` for its duration and returns it
/// alongside the read result. This satisfies the Pin requirements
/// for self-referential state machines.
type ReadFuture = Pin<Box<dyn Future<Output = (PoolGuard, Result<Frame, ClientError>)> + Send>>;

enum StreamState<T> {
    /// Ready to start the next read; carries the parked guard.
    Idle(PoolGuard),
    /// A read is in flight.
    Reading(ReadFuture),
    /// Items decoded from the current frame, waiting to be yielded.
    Buffered {
        guard: PoolGuard,
        items: VecDeque<T>,
        /// True if the frame whose items we're draining had EOS.
        ended: bool,
    },
    /// Terminal state. The guard has been dropped already.
    Ended,
    /// Temporary slot used while transitioning; never observed
    /// outside `poll_next`.
    Transitioning,
}

/// Streaming response engine. Each `Stream::poll_next` produces
/// one `Result<T, ClientError>`.
pub struct FrameStream<T> {
    /// The wire stream id assigned at request time. Surfaced to
    /// callers so they can send `UnsubscribeRequest { target_stream_id }`
    /// over a separate connection — server-side the registry key is
    /// global per shard (`crates/brain-ops/src/ops/subscribe.rs`
    /// `subscriptions.cancel(target)`).
    stream_id: u32,
    expected_opcode: Opcode,
    decoder: StreamDecoder<T>,
    state: StreamState<T>,
}

impl<T> FrameStream<T> {
    /// Construct a fresh stream around a connection that's
    /// already had its request frame written.
    pub(crate) fn new(
        guard: PoolGuard,
        stream_id: u32,
        expected_opcode: Opcode,
        decoder: StreamDecoder<T>,
    ) -> Self {
        Self {
            stream_id,
            expected_opcode,
            decoder,
            state: StreamState::Idle(guard),
        }
    }

    /// Connection-relative stream id this stream rode in on. Callers
    /// that need to send an explicit cancel (e.g. `UnsubscribeRequest`
    /// for SUBSCRIBE) use this as `target_stream_id`.
    #[must_use]
    pub fn stream_id(&self) -> u32 {
        self.stream_id
    }
}

impl<T> Drop for FrameStream<T> {
    /// If we still hold a pool guard at drop time AND the server
    /// hasn't signalled EOS, the in-flight bytes on the wire would
    /// poison the next op that acquires the same pool slot. Mark
    /// the guard failed so the slot is discarded.
    ///
    /// Note: the `Reading` state's pool guard lives inside the
    /// pinned future. When `select!` cancels mid-read, that future
    /// is dropped and the guard inside it is released *without*
    /// being marked failed. Callers who care about the lurking
    /// race should send an `UnsubscribeRequest` (or equivalent
    /// op-level cancel) before letting the stream go.
    fn drop(&mut self) {
        match std::mem::replace(&mut self.state, StreamState::Ended) {
            StreamState::Idle(mut g) => {
                g.mark_failed();
                drop(g);
            }
            StreamState::Buffered { mut guard, .. } => {
                guard.mark_failed();
                drop(guard);
            }
            StreamState::Reading(_) | StreamState::Ended | StreamState::Transitioning => {
                // Reading: guard is inside the future; future-drop
                // releases it (without mark_failed). Documented above.
                // Ended/Transitioning: no guard to release.
            }
        }
    }
}

impl<T: Unpin> Stream for FrameStream<T> {
    type Item = Result<T, ClientError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        loop {
            match std::mem::replace(&mut this.state, StreamState::Transitioning) {
                StreamState::Ended => {
                    this.state = StreamState::Ended;
                    return Poll::Ready(None);
                }
                StreamState::Transitioning => {
                    // Programmer error: we always swap back something concrete.
                    unreachable!("StreamState::Transitioning observed across poll_next branches");
                }
                StreamState::Buffered {
                    guard,
                    mut items,
                    ended,
                } => {
                    if let Some(item) = items.pop_front() {
                        if !items.is_empty() {
                            this.state = StreamState::Buffered {
                                guard,
                                items,
                                ended,
                            };
                        } else if ended {
                            // Drop the guard now — the stream is finished.
                            drop(guard);
                            this.state = StreamState::Ended;
                        } else {
                            this.state = StreamState::Idle(guard);
                        }
                        return Poll::Ready(Some(Ok(item)));
                    }
                    // No items in the current frame's batch. If the
                    // server sent an empty non-final frame (unusual
                    // but legal), loop back to read another.
                    if ended {
                        drop(guard);
                        this.state = StreamState::Ended;
                        return Poll::Ready(None);
                    }
                    this.state = StreamState::Idle(guard);
                    continue;
                }
                StreamState::Idle(mut guard) => {
                    // Spawn a fresh read future. The future owns
                    // the guard while in flight so we satisfy Send.
                    let fut: ReadFuture = Box::pin(async move {
                        let res = read_one_frame(guard.stream_mut()).await;
                        (guard, res)
                    });
                    this.state = StreamState::Reading(fut);
                    // Fall through to poll the freshly-created future.
                }
                StreamState::Reading(mut fut) => {
                    match fut.as_mut().poll(cx) {
                        Poll::Pending => {
                            this.state = StreamState::Reading(fut);
                            return Poll::Pending;
                        }
                        Poll::Ready((mut guard, Ok(frame))) => {
                            // Validate opcode + EOS, decode, transition
                            // to Buffered.
                            if frame.header.opcode_u16() == Opcode::Error.as_u16() {
                                // Server-side error response — the
                                // connection itself is fine, just the
                                // op was rejected. Don't poison the
                                // pool slot. (Drop is fine: stream
                                // ended, guard returns to Idle.)
                                drop(guard);
                                this.state = StreamState::Ended;
                                return Poll::Ready(Some(Err(map_error_frame(&frame.payload))));
                            }
                            if frame.header.opcode_u16() != this.expected_opcode.as_u16() {
                                // Byte stream is desynced — we read a
                                // valid frame but with the wrong
                                // opcode. Future ops on this conn
                                // can't reliably parse subsequent
                                // frames. Discard the slot.
                                guard.mark_failed();
                                drop(guard);
                                this.state = StreamState::Ended;
                                return Poll::Ready(Some(Err(ClientError::Protocol(
                                    ProtocolError::BadFrame(format!(
                                        "expected opcode 0x{:02x}, got 0x{:02x}",
                                        this.expected_opcode.as_u16(),
                                        frame.header.opcode_u16()
                                    )),
                                ))));
                            }
                            let ended = frame.header.flags_u8() & FLAG_EOS != 0;
                            let items = match (this.decoder)(&frame.payload) {
                                Ok(v) => v,
                                Err(e) => {
                                    // Payload didn't decode. Treat the
                                    // stream as broken: subsequent
                                    // frames can't be trusted to
                                    // realign.
                                    guard.mark_failed();
                                    drop(guard);
                                    this.state = StreamState::Ended;
                                    return Poll::Ready(Some(Err(e)));
                                }
                            };
                            this.state = StreamState::Buffered {
                                guard,
                                items: items.into(),
                                ended,
                            };
                            // Loop back to pop the first item.
                        }
                        Poll::Ready((mut guard, Err(e))) => {
                            // read_one_frame failed — Io / Closed /
                            // Protocol. All connection-fatal: the
                            // socket is unusable.
                            guard.mark_failed();
                            drop(guard);
                            this.state = StreamState::Ended;
                            return Poll::Ready(Some(Err(e)));
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // FrameStream's machinery requires a live PoolGuard, which
    // can only be constructed via Pool::acquire. End-to-end
    // invariants are exercised by tests/pool.rs and the per-op
    // streaming tests; this module keeps shape assertions only.

    #[test]
    fn type_shape() {
        let _ = std::marker::PhantomData::<FrameStream<u32>>;
    }

    #[test]
    fn frame_stream_exposes_stream_id_field() {
        // Compile-time check: the public getter exists and returns
        // u32. Behaviour is exercised through the SDK integration
        // tests once a connected pool is available.
        fn _assert<T>(s: &FrameStream<T>) -> u32 {
            s.stream_id()
        }
        let _: fn(&FrameStream<u32>) -> u32 = _assert;
    }
}
