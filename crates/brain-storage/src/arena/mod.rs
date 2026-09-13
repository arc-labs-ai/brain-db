//! Memory-mapped vector arena.
//!
//! Exposes the slot-level POD types, the file header, mmap open/grow,
//! and the allocator.

pub mod allocator;
pub mod file;
pub mod slot;

pub use allocator::{AllocError, FreeError, SlotAllocator};
pub use file::{
    ArenaFile, ArenaGrowError, ArenaOpenError, DEFAULT_INITIAL_CAPACITY_SLOTS, FORMAT_VERSION_V1,
    HEADER_CRC_COVERAGE_END, HEADER_LEN, HEADER_MAGIC, SLOT_SIZE_V1, VECTOR_DIM_V1,
};
pub use slot::{
    flags, Slot, SlotMeta, META_BYTES, META_CRC_COVERAGE_END, META_OFFSET_IN_SLOT, SLOT_ALIGN,
    SLOT_SIZE, VECTOR_BYTES, VECTOR_DIM,
};
