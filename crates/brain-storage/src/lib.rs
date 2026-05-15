//! # brain-storage
//!
//! The durable storage layer: a memory-mapped vector arena and a
//! write-ahead log (WAL).
//!
//! See `spec/05_storage_arena_wal/` for the authoritative design.
//!
//! - **Arena**: 1600-byte slots (1536 vector + 64 metadata), 64-byte aligned.
//!   Per-slot CRC32C. Allocator uses a per-shard free list with version
//!   bumping on reclamation.
//! - **WAL**: per-shard, O_DIRECT, 256 MiB segments, group commit via
//!   `pwritev2(RWF_DSYNC)`. Records have CRC32C and an LSN.
//!
//! This crate is the only place in the workspace allowed to use `unsafe`,
//! and only for memory-mapping operations.
//!
//! # Platform
//!
//! This crate targets Linux exclusively. The runtime relies on
//! `mmap`/`mremap`, `O_DIRECT`, `pwritev2(RWF_DSYNC)`, and `io_uring` —
//! none of which have a sane cross-platform substitute. Building on
//! macOS, Windows, or BSD intentionally fails at compile time so the
//! breakage is loud, not a silent fallback.
//!
//! See `README.md` → "Development environment" for the dev container.

#![allow(
    clippy::module_name_repetitions,
    clippy::missing_errors_doc,
    clippy::missing_panics_doc
)]
// `unsafe` allowed only here — needed for mmap. Keep its blast radius minimal.

#[cfg(not(target_os = "linux"))]
compile_error!(
    "brain-storage requires Linux (mmap/mremap, O_DIRECT, pwritev2(RWF_DSYNC), io_uring). \
     Build inside the dev container — see README.md \"Development environment\"."
);

#[cfg(not(target_endian = "little"))]
compile_error!(
    "brain-storage requires a little-endian target. Storage on disk is LE \
     (spec §05/02 §2); we rely on native-order memory access through \
     bytemuck::Pod, which would silently produce wrong files on a BE host."
);

pub mod arena;
pub mod layout;
pub mod recovery;
pub mod wal;

pub use layout::{ShardPaths, ensure_dirs};

/// Slot size in bytes, per `spec/05_storage_arena_wal/02_arena_layout.md`.
pub const SLOT_SIZE_BYTES: usize = 1600;

/// Slot alignment in bytes.
pub const SLOT_ALIGN_BYTES: usize = 64;

/// Default WAL segment size (256 MiB).
pub const WAL_SEGMENT_SIZE_BYTES: usize = 256 * 1024 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_size_is_1600() {
        assert_eq!(SLOT_SIZE_BYTES, 1600);
    }

    #[test]
    fn slot_alignment_divides_size() {
        assert_eq!(SLOT_SIZE_BYTES % SLOT_ALIGN_BYTES, 0);
    }
}
