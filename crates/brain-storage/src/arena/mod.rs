//! Memory-mapped vector arena.
//!
//! See `spec/05_storage_arena_wal/02_arena_layout.md` for the authoritative
//! byte-level layout. This module currently exposes the slot-level POD
//! types (sub-task 2.3); the file header, mmap open/grow, and the
//! allocator land in subsequent sub-tasks (2.4–2.5).

pub mod slot;

pub use slot::{
    flags, Slot, SlotMeta, META_BYTES, META_CRC_COVERAGE_END, META_OFFSET_IN_SLOT, SLOT_ALIGN,
    SLOT_SIZE, VECTOR_BYTES, VECTOR_DIM,
};
