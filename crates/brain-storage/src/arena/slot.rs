//! Arena slot byte layout.
//!
//! Layout per `spec/05_storage_arena_wal/02_arena_layout.md` §§3–4:
//!
//! ```text
//! Slot (1600 bytes, 64-byte aligned):
//!    0..1536    vector       384 × f32 LE
//! 1536..1600    metadata     SlotMeta (64 bytes)
//!
//! SlotMeta (64 bytes, offsets within metadata):
//!    0..4       slot_version              u32 LE
//!    4..8       flags                     u32 LE
//!    8..24      embedding_model_fp_short  [u8; 16]
//!   24..32      created_at_unix_nanos     u64 LE
//!   32..40      last_modified_at_unix_ns  u64 LE
//!   40..44      metadata_crc32c           u32 LE
//!   44..64      reserved                  [u8; 20]
//! ```
//!
//! ## CRC coverage
//!
//! Spec §3.2 prints "metadata bytes [0..36]" but byte 36 splits the
//! `last_modified_at` u64 (which spans 32..40). Almost certainly a typo.
//! We cover `[0..40]` of the metadata — every field before
//! `metadata_crc32c` itself — plus the 1536 vector bytes. This matches the
//! more general "CRC excludes only itself + reserved" pattern used
//! elsewhere in the spec (e.g. arena header, WAL record footer).
//!
//! ## What's *not* in SlotMeta
//!
//! `agent_id`, `context_id`, `kind`, `salience`, and `text` live in the
//! metadata store (redb), not the arena. The arena holds the vector and
//! the bookkeeping needed to validate it (version, flags, fp, timestamps,
//! CRC). The phase doc's task-2.3 sketch lists agent/context/kind/salience
//! as slot fields — that's at odds with the spec; the spec wins.
//!
//! ## What's *not* enforced here
//!
//! - **Vector finiteness / L2 norm** (spec §3.1). Bytemuck::Pod admits any
//!   bit pattern; finite/normalized validation is a separate helper added
//!   alongside the encode path.
//! - **CRC freshness on read.** Computing `metadata_crc32c` on every read
//!   would ruin the hot path; it's verified during scrubbing or when
//!   recovery suspects a slot.

use core::mem::{align_of, offset_of, size_of};

/// Vector length, in f32 elements, for the v1 BGE-small model.
pub const VECTOR_DIM: usize = 384;

/// Vector size in bytes (`VECTOR_DIM * 4`).
pub const VECTOR_BYTES: usize = VECTOR_DIM * size_of::<f32>();

/// Slot metadata size in bytes (spec §3.2).
pub const META_BYTES: usize = 64;

/// Total slot size in bytes (spec §3).
pub const SLOT_SIZE: usize = VECTOR_BYTES + META_BYTES;

/// Slot alignment in bytes (spec §4 — cache-line aligned).
pub const SLOT_ALIGN: usize = 64;

/// Byte offset of the metadata block within a slot.
pub const META_OFFSET_IN_SLOT: usize = VECTOR_BYTES;

/// End of the CRC-covered region within metadata.
///
/// CRC32C covers metadata bytes `[0..META_CRC_COVERAGE_END]` plus the full
/// vector. Spec §3.2's literal "[0..36]" splits `last_modified_at` mid-field;
/// `[0..40]` is the defensible reading.
pub const META_CRC_COVERAGE_END: usize = 40;

/// Bit-flag definitions for `SlotMeta::flags` (spec §3.2).
pub mod flags {
    /// Slot occupies a memory (1 = occupied, 0 = free).
    pub const OCCUPIED: u32 = 1 << 0;
    /// Memory has been forgotten and is awaiting reclamation.
    pub const TOMBSTONED: u32 = 1 << 1;
    /// Write is in progress (transient).
    pub const PENDING_WRITE: u32 = 1 << 2;
    /// Vector was zeroed by hard-forget (informational).
    pub const HARD_FORGOTTEN: u32 = 1 << 3;

    /// Mask of bits that v1 leaves reserved (must be zero on write,
    /// ignored on read).
    pub const RESERVED_MASK: u32 = !(OCCUPIED | TOMBSTONED | PENDING_WRITE | HARD_FORGOTTEN);
}

/// Per-slot metadata. 64 bytes, `#[repr(C)]`, no implicit padding.
///
/// All multi-byte scalars are little-endian on disk; we rely on native-order
/// memory access (the crate is gated to LE Linux at the lib root).
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SlotMeta {
    pub slot_version: u32,
    pub flags: u32,
    pub embedding_model_fp_short: [u8; 16],
    pub created_at_unix_nanos: u64,
    pub last_modified_at_unix_nanos: u64,
    pub metadata_crc32c: u32,
    pub reserved: [u8; 20],
}

// SAFETY: SlotMeta is `#[repr(C)]`, contains only `Pod` fields (integers and
// byte arrays), and has no implicit padding (verified by the static_assert!
// of `size_of` below). All bit patterns of every field are valid values.
unsafe impl bytemuck::Zeroable for SlotMeta {}
unsafe impl bytemuck::Pod for SlotMeta {}

/// One arena slot. 1600 bytes, 64-byte aligned, `#[repr(C)]`, no implicit
/// padding.
#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct Slot {
    pub vector: [f32; VECTOR_DIM],
    pub metadata: SlotMeta,
}

// SAFETY: Slot is `#[repr(C, align(64))]`, contains only `Pod` fields
// (`[f32; 384]` and `SlotMeta`), and has no implicit padding (vector is
// 1536 bytes = 192·8, then SlotMeta starts at an 8-aligned offset; total
// 1600 = 25·64 is a multiple of the 64-byte alignment). All bit patterns
// of every field are valid values (f32 admits NaN/Inf/subnormals as
// representable bit patterns).
unsafe impl bytemuck::Zeroable for Slot {}
unsafe impl bytemuck::Pod for Slot {}

// Compile-time invariants.
const _: () = {
    assert!(size_of::<SlotMeta>() == META_BYTES);
    assert!(align_of::<SlotMeta>() == 8);
    assert!(size_of::<Slot>() == SLOT_SIZE);
    assert!(align_of::<Slot>() == SLOT_ALIGN);
    assert!(offset_of!(Slot, metadata) == META_OFFSET_IN_SLOT);
    assert!(offset_of!(SlotMeta, metadata_crc32c) == META_CRC_COVERAGE_END);
};

impl Slot {
    /// All-zero slot. Equivalent to `bytemuck::Zeroable::zeroed()`; provided
    /// as an inherent method for ergonomics.
    #[must_use]
    pub fn zeroed() -> Self {
        bytemuck::Zeroable::zeroed()
    }

    #[must_use]
    pub fn is_occupied(&self) -> bool {
        self.metadata.flags & flags::OCCUPIED != 0
    }

    #[must_use]
    pub fn is_tombstoned(&self) -> bool {
        self.metadata.flags & flags::TOMBSTONED != 0
    }

    #[must_use]
    pub fn is_pending_write(&self) -> bool {
        self.metadata.flags & flags::PENDING_WRITE != 0
    }

    #[must_use]
    pub fn is_hard_forgotten(&self) -> bool {
        self.metadata.flags & flags::HARD_FORGOTTEN != 0
    }

    /// Set or clear a single flag bit (or a combination via `|`).
    pub fn set_flag(&mut self, mask: u32, on: bool) {
        if on {
            self.metadata.flags |= mask;
        } else {
            self.metadata.flags &= !mask;
        }
    }

    /// Compute the CRC32C of this slot's covered region.
    ///
    /// Coverage: vector bytes (1536) + metadata bytes `[0..40]`. Excludes
    /// `metadata_crc32c` itself and the 20-byte `reserved` tail. See module
    /// docs for the rationale.
    #[must_use]
    pub fn compute_crc(&self) -> u32 {
        let bytes: &[u8] = bytemuck::bytes_of(self);
        // First the vector, then the metadata prefix that the CRC covers.
        // We chain via `crc32c_append` so the bytes are hashed contiguously
        // from the hasher's perspective — same result as concatenating into
        // one buffer, but without the allocation.
        let crc_vec = crc32c::crc32c(&bytes[0..VECTOR_BYTES]);
        crc32c::crc32c_append(
            crc_vec,
            &bytes[META_OFFSET_IN_SLOT..META_OFFSET_IN_SLOT + META_CRC_COVERAGE_END],
        )
    }

    /// Recompute and store `metadata.metadata_crc32c`. Call after every
    /// write that mutates a CRC-covered field.
    pub fn refresh_crc(&mut self) {
        self.metadata.metadata_crc32c = self.compute_crc();
    }

    /// Check whether the stored CRC matches the computed one. `true` for a
    /// freshly-`refresh_crc`'d slot; `false` if any covered byte was
    /// mutated without refreshing.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        self.metadata.metadata_crc32c == self.compute_crc()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    // ----- Layout invariants -----------------------------------------------

    #[test]
    fn meta_size_is_64() {
        assert_eq!(size_of::<SlotMeta>(), META_BYTES);
    }

    #[test]
    fn slot_size_is_1600() {
        assert_eq!(size_of::<Slot>(), SLOT_SIZE);
    }

    #[test]
    fn slot_alignment_is_64() {
        assert_eq!(align_of::<Slot>(), SLOT_ALIGN);
    }

    #[test]
    fn metadata_starts_at_byte_1536() {
        assert_eq!(offset_of!(Slot, metadata), VECTOR_BYTES);
    }

    #[test]
    fn metadata_crc_field_at_offset_40() {
        assert_eq!(offset_of!(SlotMeta, metadata_crc32c), META_CRC_COVERAGE_END);
    }

    #[test]
    fn reserved_mask_covers_high_bits() {
        // Bits 0..=3 are spec'd; bits 4..=31 are reserved.
        assert_eq!(flags::RESERVED_MASK & 0xFu32, 0);
        assert_eq!(flags::RESERVED_MASK | 0xFu32, u32::MAX);
    }

    // ----- CRC behavior ----------------------------------------------------

    fn populated_slot() -> Slot {
        let mut s = Slot::zeroed();
        // Fill the vector with a recognizable ramp.
        for (i, x) in s.vector.iter_mut().enumerate() {
            *x = (i as f32) * 0.001;
        }
        s.metadata.slot_version = 7;
        s.metadata.flags = flags::OCCUPIED;
        s.metadata.embedding_model_fp_short = [0xAA; 16];
        s.metadata.created_at_unix_nanos = 1_700_000_000_000_000_000;
        s.metadata.last_modified_at_unix_nanos = 1_700_000_000_000_000_001;
        s.refresh_crc();
        s
    }

    #[test]
    fn round_trip_is_valid() {
        let s = populated_slot();
        assert!(s.is_valid());
    }

    #[test]
    fn vector_corruption_invalidates_crc() {
        let mut s = populated_slot();
        // Flip a bit in vector element 0's f32 representation.
        let bytes = bytemuck::bytes_of_mut(&mut s);
        bytes[0] ^= 0x01;
        assert!(!s.is_valid());
    }

    #[test]
    fn metadata_corruption_in_covered_range_invalidates() {
        let mut s = populated_slot();
        // Flip the low byte of slot_version (covered by CRC).
        let bytes = bytemuck::bytes_of_mut(&mut s);
        bytes[META_OFFSET_IN_SLOT] ^= 0x01;
        assert!(!s.is_valid());
    }

    #[test]
    fn metadata_corruption_in_reserved_does_not_invalidate() {
        let mut s = populated_slot();
        // Reserved spans metadata bytes 44..64 (slot bytes 1580..1600).
        let bytes = bytemuck::bytes_of_mut(&mut s);
        bytes[META_OFFSET_IN_SLOT + 50] ^= 0xFF;
        bytes[SLOT_SIZE - 1] ^= 0xFF;
        assert!(s.is_valid(), "CRC must exclude reserved bytes");
    }

    #[test]
    fn crc_excludes_itself() {
        // Mutating metadata_crc32c must not change `compute_crc`'s output.
        let s = populated_slot();
        let crc_before = s.compute_crc();
        let mut s2 = s;
        s2.metadata.metadata_crc32c = !s.metadata.metadata_crc32c;
        let crc_after = s2.compute_crc();
        assert_eq!(crc_before, crc_after);
    }

    // ----- Flag accessors --------------------------------------------------

    #[test]
    fn zeroed_slot_has_no_flags_set() {
        let s = Slot::zeroed();
        assert!(!s.is_occupied());
        assert!(!s.is_tombstoned());
        assert!(!s.is_pending_write());
        assert!(!s.is_hard_forgotten());
    }

    #[test]
    fn set_flag_sets_only_target_bit() {
        let mut s = Slot::zeroed();
        s.set_flag(flags::OCCUPIED, true);
        assert!(s.is_occupied());
        assert!(!s.is_tombstoned());
        assert!(!s.is_pending_write());
        assert!(!s.is_hard_forgotten());
        assert_eq!(s.metadata.flags, flags::OCCUPIED);
    }

    #[test]
    fn set_flag_off_is_idempotent() {
        let mut s = Slot::zeroed();
        s.set_flag(flags::TOMBSTONED, true);
        s.set_flag(flags::TOMBSTONED, false);
        s.set_flag(flags::TOMBSTONED, false);
        assert!(!s.is_tombstoned());
        assert_eq!(s.metadata.flags, 0);
    }

    #[test]
    fn occupied_and_tombstoned_coexist() {
        // Spec §3.2: "bit 0 = 1, bit 1 = 1" is the active-but-tombstoned state.
        let mut s = Slot::zeroed();
        s.set_flag(flags::OCCUPIED | flags::TOMBSTONED, true);
        assert!(s.is_occupied());
        assert!(s.is_tombstoned());
    }

    // ----- Endianness sanity ----------------------------------------------

    #[test]
    fn slot_version_is_stored_little_endian() {
        let mut s = Slot::zeroed();
        s.metadata.slot_version = 0x0102_0304;
        let bytes = bytemuck::bytes_of(&s);
        // slot_version is at metadata-offset 0 = slot-offset 1536.
        assert_eq!(
            &bytes[META_OFFSET_IN_SLOT..META_OFFSET_IN_SLOT + 4],
            &[0x04, 0x03, 0x02, 0x01],
            "must be stored little-endian per spec §05/02 §2",
        );
    }

    // ----- Property test: CRC stability under uncovered-region mutation ---

    proptest! {
        #[test]
        fn crc_invariant_under_uncovered_mutations(
            // Mutate any of the 24 bytes in slot bytes 1576..1600
            // (metadata_crc32c + reserved). `compute_crc` must not change.
            crc_field in any::<u32>(),
            reserved in any::<[u8; 20]>(),
        ) {
            let s = populated_slot();
            let crc_baseline = s.compute_crc();

            let mut s2 = s;
            s2.metadata.metadata_crc32c = crc_field;
            s2.metadata.reserved = reserved;
            prop_assert_eq!(s2.compute_crc(), crc_baseline);
        }
    }
}
