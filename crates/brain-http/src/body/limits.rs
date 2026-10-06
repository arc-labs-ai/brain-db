//! Bounded body reader.
//!
//! Mitigates a trivial-DoS pattern:
//! a malicious client declares a huge `Content-Length` (or sends an
//! unbounded chunked body), and a naive `collect().await` OOMs trying
//! to buffer it. [`read_to_bytes`] consults `size_hint().upper()`
//! BEFORE allocating for a cheap early reject, then *streams* the body
//! frame-by-frame with a running byte cap — so a chunked / unknown-
//! length body (whose `upper()` is `None`) or one that lies about its
//! hint is aborted the moment the accumulated size crosses `limit`,
//! never fully buffered.

use bytes::{Bytes, BytesMut};
use http_body::Body;
use http_body_util::BodyExt;

/// 16 MiB. Matches the existing admin server's implicit ceiling. The
/// limit can be overridden per call.
pub const MAX_BODY_BYTES: u64 = 16 * 1024 * 1024;

/// Collect a body into a contiguous [`Bytes`], rejecting bodies that
/// would exceed `limit`.
///
/// The rejection happens in two places:
///
/// 1. Before any buffering, if `body.size_hint().upper()` indicates
///    a length above `limit`. This is the cheap path — no bytes
///    move at all.
/// 2. While streaming, comparing the running accumulated length
///    against `limit` after each data frame. This bounds chunked /
///    unknown-length bodies (whose `upper()` is `None`) and ones that
///    lie about their size hint: the read is aborted as soon as the
///    accumulated size crosses `limit`, so at most `limit` bytes plus
///    one frame are ever buffered.
///
/// # Errors
///
/// Returns [`crate::Error::BodyTooLarge`] on overflow,
/// [`crate::Error::Hyper`] or the body's own error type on read
/// failure.
pub async fn read_to_bytes<B>(body: B, limit: u64) -> crate::Result<Bytes>
where
    B: Body<Data = Bytes>,
    B::Error: Into<crate::Error>,
{
    // Cheap path: trust the size hint upper bound when present.
    if let Some(upper) = body.size_hint().upper() {
        if upper > limit {
            return Err(crate::Error::BodyTooLarge {
                actual: upper,
                limit,
            });
        }
    }

    // Streaming path: accumulate frame-by-frame, aborting the instant
    // the running total exceeds `limit`. This is what actually bounds
    // an unbounded chunked body — never trust the hint alone.
    let mut acc = BytesMut::new();
    let mut accumulated: u64 = 0;
    let mut body = std::pin::pin!(body);
    while let Some(frame) = body.as_mut().frame().await {
        let frame = frame.map_err(Into::into)?;
        // Trailers and other non-data frames carry no body bytes.
        let Ok(data) = frame.into_data() else {
            continue;
        };
        accumulated = accumulated.saturating_add(data.len() as u64);
        if accumulated > limit {
            return Err(crate::Error::BodyTooLarge {
                actual: accumulated,
                limit,
            });
        }
        acc.extend_from_slice(&data);
    }

    Ok(acc.freeze())
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::Full;

    #[tokio::test]
    async fn accepts_under_limit() {
        let body = Full::new(Bytes::from_static(b"hello"));
        let out = read_to_bytes(body, 100).await.expect("ok");
        assert_eq!(out.as_ref(), b"hello");
    }

    #[tokio::test]
    async fn rejects_over_limit_via_size_hint() {
        // Full advertises its exact size; we expect early rejection
        // without ever buffering.
        let body = Full::new(Bytes::from(vec![0u8; 2000]));
        let err = read_to_bytes(body, 1000).await.expect_err("err");
        assert!(matches!(
            err,
            crate::Error::BodyTooLarge {
                actual: 2000,
                limit: 1000
            }
        ));
    }

    #[tokio::test]
    async fn accepts_exactly_at_limit() {
        let body = Full::new(Bytes::from(vec![0u8; 1000]));
        let out = read_to_bytes(body, 1000).await.expect("ok");
        assert_eq!(out.len(), 1000);
    }

    /// A body whose `size_hint().upper()` is `None` — models a chunked
    /// request with no declared length. Streaming multiple data frames
    /// makes hyper report an unknown upper bound, so the cheap
    /// early-reject cannot fire and the running cap is what must bound
    /// it.
    fn unknown_length_body(
        chunks: Vec<Bytes>,
    ) -> impl Body<Data = Bytes, Error = std::convert::Infallible> {
        let frames: Vec<Result<http_body::Frame<Bytes>, std::convert::Infallible>> = chunks
            .into_iter()
            .map(|c| Ok(http_body::Frame::data(c)))
            .collect();
        http_body_util::StreamBody::new(futures_util::stream::iter(frames))
    }

    #[tokio::test]
    async fn unknown_length_body_has_no_upper_hint() {
        // Guards the premise of the streaming path: a multi-frame
        // stream body reports `upper() == None`, so the cheap check is
        // bypassed and only the running cap protects us.
        let body =
            unknown_length_body(vec![Bytes::from(vec![0u8; 10]), Bytes::from(vec![0u8; 10])]);
        assert_eq!(body.size_hint().upper(), None);
    }

    #[tokio::test]
    async fn rejects_over_limit_chunked_without_size_hint() {
        // Three 500-byte frames = 1500 bytes with no declared length.
        // The size-hint early reject can't fire; the running cap must.
        let body = unknown_length_body(vec![
            Bytes::from(vec![1u8; 500]),
            Bytes::from(vec![2u8; 500]),
            Bytes::from(vec![3u8; 500]),
        ]);
        let err = read_to_bytes(body, 1000).await.expect_err("err");
        // `actual` is the running total at the point it crossed the cap
        // (after the third frame: 1500), not a trusted hint.
        assert!(
            matches!(
                err,
                crate::Error::BodyTooLarge {
                    actual: 1500,
                    limit: 1000
                }
            ),
            "unexpected error: {err:?}"
        );
    }

    #[tokio::test]
    async fn accepts_under_limit_chunked_without_size_hint() {
        let body = unknown_length_body(vec![
            Bytes::from_static(b"hel"),
            Bytes::from_static(b"lo "),
            Bytes::from_static(b"world"),
        ]);
        let out = read_to_bytes(body, 1000).await.expect("ok");
        assert_eq!(out.as_ref(), b"hello world");
    }
}
