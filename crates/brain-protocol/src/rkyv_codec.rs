//! Shared rkyv encode/decode helpers for request and response bodies.
//!
//! Both `crate::request` and `crate::response` use the same rkyv 0.7
//! pipeline: serialize with `AllocSerializer`, validate-and-deserialize
//! with `check_archived_root` + `Infallible`. The helpers here factor
//! that out so the two body modules don't drift.

use rkyv::ser::serializers::AllocSerializer;
use rkyv::ser::Serializer as _;
use rkyv::{Archive, Deserialize, Infallible, Serialize};

use crate::error::ProtocolError;

/// Initial scratch-buffer size for the rkyv `AllocSerializer`. This is
/// just the *starting* allocation; rkyv grows the buffer as needed, so
/// 256 covers small payloads without forcing reallocation while staying
/// small for ping-sized messages.
pub(crate) const RKYV_SCRATCH: usize = 256;

/// Serialize a single rkyv-archivable value into a freshly allocated byte
/// vector. Encoding never fails for our body types (no IO, just memory
/// allocation), so the helper unwraps the unreachable error path with a
/// descriptive message.
pub(crate) fn to_rkyv_bytes<T>(value: &T) -> Vec<u8>
where
    T: Serialize<AllocSerializer<RKYV_SCRATCH>>,
{
    let mut serializer = AllocSerializer::<RKYV_SCRATCH>::default();
    serializer
        .serialize_value(value)
        .expect("invariant: rkyv allocator is infallible for our body types");
    serializer.into_serializer().into_inner().to_vec()
}

/// Validate `bytes` as an archived `T` and deserialize an owned copy.
/// Both validation and deserialization failures are surfaced as
/// [`ProtocolError::MalformedPayload`].
pub(crate) fn from_rkyv_bytes<T>(bytes: &[u8]) -> Result<T, ProtocolError>
where
    T: Archive,
    T::Archived: for<'a> rkyv::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>
        + Deserialize<T, Infallible>,
{
    let archived = rkyv::check_archived_root::<T>(bytes)
        .map_err(|e| ProtocolError::MalformedPayload(format!("rkyv check failed: {e}")))?;
    archived
        .deserialize(&mut Infallible)
        .map_err(|_: core::convert::Infallible| {
            ProtocolError::MalformedPayload("rkyv deserialize failed".into())
        })
}
