//! `RequestIdSource` — pluggable generator for the per-call
//! [`brain_core::RequestId`].
//!
//! Spec §13/04 §3 ties idempotency to a UUIDv7 `RequestId` on
//! every state-mutating op (ENCODE / FORGET / LINK / UNLINK /
//! TXN_COMMIT). The SDK generates one automatically if the caller
//! didn't supply one, and **reuses the same id across retries** so
//! the server's 24-hour idempotency cache (per spec §07/06 §3)
//! deduplicates correctly.
//!
//! `DefaultRequestIdSource` wraps `brain_core::RequestId::new()`
//! (UUIDv7). `FixedRequestIdSource` (test-only) returns a canned
//! sequence so wire-shape assertions are deterministic.

use brain_core::RequestId;

/// Pluggable RequestId generator. The SDK calls
/// [`RequestIdSource::next`] once per op-method invocation; the
/// retry runner reuses that id across attempts.
pub trait RequestIdSource: Send + Sync + 'static {
    /// Return a fresh `RequestId`.
    fn next(&self) -> RequestId;
}

/// Production source: every call returns
/// `RequestId::new()` (UUIDv7, time-ordered).
#[derive(Default)]
pub struct DefaultRequestIdSource;

impl RequestIdSource for DefaultRequestIdSource {
    fn next(&self) -> RequestId {
        RequestId::new()
    }
}

/// Test-only source that returns a canned sequence of ids and
/// panics once exhausted. Use when a test needs to assert on the
/// exact RequestId bytes that hit the wire.
#[cfg(test)]
pub(crate) struct FixedRequestIdSource {
    queue: std::sync::Mutex<std::collections::VecDeque<RequestId>>,
}

#[cfg(test)]
impl FixedRequestIdSource {
    pub(crate) fn new(ids: Vec<RequestId>) -> Self {
        Self {
            queue: std::sync::Mutex::new(ids.into_iter().collect()),
        }
    }
}

#[cfg(test)]
impl RequestIdSource for FixedRequestIdSource {
    fn next(&self) -> RequestId {
        self.queue
            .lock()
            .expect("FixedRequestIdSource mutex poisoned")
            .pop_front()
            .expect("FixedRequestIdSource: queue exhausted")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tag bits per RFC 9562: UUIDv7 has version `0b0111` in the
    /// `M` nibble (byte 6 high nibble) and variant `0b10xx` in
    /// byte 8 high two bits.
    fn is_uuidv7(id: RequestId) -> bool {
        let bytes: [u8; 16] = id.into();
        let version = bytes[6] >> 4;
        let variant = bytes[8] >> 6;
        version == 7 && variant == 0b10
    }

    #[test]
    fn default_source_returns_distinct_uuidv7s() {
        let src = DefaultRequestIdSource;
        let a = src.next();
        let b = src.next();
        let c = src.next();
        assert!(is_uuidv7(a) && is_uuidv7(b) && is_uuidv7(c));
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }

    #[test]
    fn fixed_source_cycles_and_then_panics() {
        let id1 = RequestId::new();
        let id2 = RequestId::new();
        let src = FixedRequestIdSource::new(vec![id1, id2]);
        assert_eq!(src.next(), id1);
        assert_eq!(src.next(), id2);
    }

    #[test]
    #[should_panic(expected = "queue exhausted")]
    fn fixed_source_panics_when_drained() {
        let src = FixedRequestIdSource::new(vec![RequestId::new()]);
        let _ = src.next();
        let _ = src.next();
    }
}
