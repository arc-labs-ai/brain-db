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
///
/// Copies `bytes` into an `AlignedVec` before validation. rkyv 0.7's
/// validator requires 8-byte alignment for archives containing u64 /
/// pointer-sized fields, and frame payloads come off a TCP buffer at
/// whatever alignment the read landed on. Today the frame layout
/// happens to put payloads at 8-byte offsets, but that's an accident
/// of the header size; one buffer-pool change away from a wire-level
/// `MalformedPayload` for a legitimate frame (worst case under the
/// old check-direct path: a panic on the redb decode side of the same
/// pattern, see `brain-metadata::tables::fingerprint::from_bytes`
/// 2026-05-20 fix). Copying into AlignedVec costs one allocation per
/// decode — negligible vs the owned-`T` allocation the deserializer
/// produces anyway, and entirely off the zero-copy read path
/// (substrate hot reads go through arena/rkyv-archived-view, not
/// through this wire-decode helper).
pub(crate) fn from_rkyv_bytes<T>(bytes: &[u8]) -> Result<T, ProtocolError>
where
    T: Archive,
    T::Archived: for<'a> rkyv::CheckBytes<rkyv::validation::validators::DefaultValidator<'a>>
        + Deserialize<T, Infallible>,
{
    let mut buf = rkyv::AlignedVec::with_capacity(bytes.len());
    buf.extend_from_slice(bytes);
    let archived = rkyv::check_archived_root::<T>(&buf)
        .map_err(|e| ProtocolError::MalformedPayload(format!("rkyv check failed: {e}")))?;
    archived
        .deserialize(&mut Infallible)
        .map_err(|_: core::convert::Infallible| {
            ProtocolError::MalformedPayload("rkyv deserialize failed".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Companion to `brain-metadata`'s `from_bytes_handles_misaligned_input`
    /// (commit e705639). Every wire decoder funnels through
    /// `from_rkyv_bytes`; if it can't tolerate arbitrarily-aligned input,
    /// any future change to how frame buffers are allocated could turn
    /// well-formed frames into `MalformedPayload` errors. Force a
    /// 1-byte alignment by slicing past a leading sentinel byte.
    #[test]
    fn from_rkyv_bytes_handles_misaligned_input() {
        // Any archivable body type with a u64-aligned field works as
        // the canary; AuthOkPayload has server_time_unix_nanos which
        // forces the 8-byte alignment requirement on the archive root.
        // The structure isn't load-bearing here — only the round-trip
        // through an intentionally-misaligned slice is.
        use crate::connection::handshake::{AgentPermissions, AuthOkPayload};

        let value = AuthOkPayload {
            agent_id: [0u8; 16],
            bound_shard_id: 0,
            permissions: AgentPermissions {
                can_encode: true,
                can_recall: true,
                can_plan: false,
                can_reason: false,
                can_forget: false,
                can_admin: false,
            },
            server_time_unix_nanos: 0x1234_5678_9abc_def0,
        };
        let bytes = to_rkyv_bytes(&value);

        // Place the payload at offset 1 of a fresh buffer to force a
        // 1-byte alignment for the &bytes[1..] slice we hand to the
        // decoder. Under the old check-direct path this would error
        // with `Underaligned { expected_align: 8, actual_align: 1 }`.
        let mut misaligned = vec![0u8; bytes.len() + 1];
        misaligned[1..].copy_from_slice(&bytes);

        let decoded: AuthOkPayload = from_rkyv_bytes(&misaligned[1..])
            .expect("misaligned slice must decode after AlignedVec copy");
        assert_eq!(decoded.server_time_unix_nanos, 0x1234_5678_9abc_def0);
    }
}
