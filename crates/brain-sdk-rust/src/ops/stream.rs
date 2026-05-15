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
    expected_opcode: Opcode,
    decoder: StreamDecoder<T>,
    state: StreamState<T>,
}

impl<T> FrameStream<T> {
    /// Construct a fresh stream around a connection that's
    /// already had its request frame written.
    pub(crate) fn new(
        guard: PoolGuard,
        expected_opcode: Opcode,
        decoder: StreamDecoder<T>,
    ) -> Self {
        Self {
            expected_opcode,
            decoder,
            state: StreamState::Idle(guard),
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
                        Poll::Ready((guard, Ok(frame))) => {
                            // Validate opcode + EOS, decode, transition
                            // to Buffered.
                            if frame.header.opcode_u16() == Opcode::Error.as_u16() {
                                drop(guard);
                                this.state = StreamState::Ended;
                                return Poll::Ready(Some(Err(map_error_frame(&frame.payload))));
                            }
                            if frame.header.opcode_u16() != this.expected_opcode.as_u16() {
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
                        Poll::Ready((guard, Err(e))) => {
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
    // can only be constructed via Pool::acquire. The stream's
    // invariants are exercised end-to-end by the integration
    // tests in `tests/ops_recall_stream.rs` etc. We keep this
    // unit module as a placeholder for type-shape assertions.
    #[test]
    fn type_shape() {
        let _ = std::marker::PhantomData::<FrameStream<u32>>;
    }
}
