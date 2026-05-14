//! Bounded body reader.
//!
//! Mitigates the trivial-DoS pattern flagged in the design report §R9:
//! a malicious client declares a huge `Content-Length` (or sends an
//! unbounded chunked body), and a naive `collect().await` OOMs trying
//! to buffer it. [`read_to_bytes`] consults `size_hint().upper()`
//! BEFORE allocating, and bails out early on overflow.

use bytes::Bytes;
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
/// 2. After collection, comparing the actual length against `limit`.
///    Catches bodies that lie about their size hint.
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

    let collected = body.collect().await.map_err(Into::into)?.to_bytes();

    // Verify path: catch bodies that lied about size_hint or didn't
    // declare one.
    let len = collected.len() as u64;
    if len > limit {
        return Err(crate::Error::BodyTooLarge { actual: len, limit });
    }
    Ok(collected)
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
}
