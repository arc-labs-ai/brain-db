//! The arena file: open, mmap, read/write slots, grow.
//!
//! See `spec/05_storage_arena_wal/{01,02,03}*.md`.
//!
//! `ArenaFile` owns one `arena.bin` and the mmap'd view of it. The file
//! has a 4096-byte header followed by a contiguous array of fixed-size
//! `Slot` (1600 bytes each).
//!
//! ## Why hand-rolled libc instead of `memmap2`
//!
//! Spec §05/03 §4 prescribes `mremap(2)` with `MREMAP_MAYMOVE` for growth.
//! `memmap2` does not expose `mremap`, so going through it would force the
//! spec's *fallback* path (§05/03 §5: unmap-then-mmap). Hand-rolling lets
//! us use the primary path and keeps a single owned region we grow in
//! place. Every `unsafe` block in this file carries a `// SAFETY:` comment
//! and is the smallest scope that compiles.
//!
//! ## Concurrency
//!
//! At this layer:
//! - `slot(&self, idx) -> &Slot` allows multiple concurrent reads of
//!   different slots through `&Arc<ArenaFile>`.
//! - `slot_mut(&mut self, idx) -> &mut Slot` and `grow_to(&mut self, ...)`
//!   require exclusive access.
//!
//! The single-writer-per-shard discipline (and the arc-swap layer that
//! lets readers and the single writer coexist on a Glommio executor) is
//! a higher-level concern, added when we wire the writer task in 2.5+.

use core::ptr::NonNull;
use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::io::AsRawFd;
use std::path::Path;

use crate::arena::slot::{Slot, SLOT_SIZE};

// ---------------------------------------------------------------------------
// Header.
// ---------------------------------------------------------------------------

/// Header size in bytes (spec §05/02 §2).
pub const HEADER_LEN: usize = 4096;

/// Magic bytes at offset 0 of the header (spec §05/02 §2).
pub const HEADER_MAGIC: [u8; 4] = *b"BARN";

/// Format version written into new arenas.
pub const FORMAT_VERSION_V1: u32 = 1;

/// v1 vector dimension. Must match the value in the header.
pub const VECTOR_DIM_V1: u32 = 384;

/// v1 slot size. Must match the value in the header.
pub const SLOT_SIZE_V1: u32 = 1600;

/// End of the CRC-covered region within the header.
///
/// Spec §05/02 §2 prints "[0..76]" but byte 76 splits the `last_grow_at`
/// u64 (offsets 72..80). Same typo pattern as the slot CRC; we read it as
/// "every header field before `header_crc32c`" — bytes `[0..80]`. See
/// `.claude/plans/phase-02-task-04.md` §3.1 for the rationale.
pub const HEADER_CRC_COVERAGE_END: usize = 80;

/// Default initial capacity for a fresh arena (spec §05/02 §9).
pub const DEFAULT_INITIAL_CAPACITY_SLOTS: u64 = 1024;

/// Test-only counter: every call to `ArenaFile::msync_all` bumps this.
/// Used by the 2.12 checkpoint test to verify the syscall fires between
/// `CHECKPOINT_BEGIN` and `CHECKPOINT_END`.
#[cfg(test)]
pub static MSYNC_ALL_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Header struct mirroring spec §05/02 §2 byte-for-byte. `#[repr(C)]`,
/// no implicit padding (verified by `bytemuck::Pod` derive and the
/// const-asserts below).
#[repr(C)]
#[derive(Clone, Copy)]
struct HeaderRaw {
    magic: [u8; 4],
    format_version: u32,
    shard_uuid: [u8; 16],
    vector_dim: u32,
    slot_size: u32,
    slot_count_capacity: u64,
    slot_count_in_use: u64,
    embedding_model_fp_active: [u8; 16],
    created_at_unix_nanos: u64,
    last_grow_at_unix_nanos: u64,
    header_crc32c: u32,
    reserved: [u8; 4012],
}

// SAFETY: HeaderRaw is `#[repr(C)]`, contains only Pod fields (integers and
// byte arrays), and has no implicit padding (size = 4096, verified by the
// const block below). All bit patterns of every field are valid values.
unsafe impl bytemuck::Zeroable for HeaderRaw {}
unsafe impl bytemuck::Pod for HeaderRaw {}

// Compile-time invariants for the header layout.
const _: () = {
    use core::mem::{align_of, offset_of, size_of};
    assert!(size_of::<HeaderRaw>() == HEADER_LEN);
    assert!(align_of::<HeaderRaw>() == 8);
    assert!(offset_of!(HeaderRaw, magic) == 0);
    assert!(offset_of!(HeaderRaw, format_version) == 4);
    assert!(offset_of!(HeaderRaw, shard_uuid) == 8);
    assert!(offset_of!(HeaderRaw, vector_dim) == 24);
    assert!(offset_of!(HeaderRaw, slot_size) == 28);
    assert!(offset_of!(HeaderRaw, slot_count_capacity) == 32);
    assert!(offset_of!(HeaderRaw, slot_count_in_use) == 40);
    assert!(offset_of!(HeaderRaw, embedding_model_fp_active) == 48);
    assert!(offset_of!(HeaderRaw, created_at_unix_nanos) == 64);
    assert!(offset_of!(HeaderRaw, last_grow_at_unix_nanos) == 72);
    assert!(offset_of!(HeaderRaw, header_crc32c) == 80);
    assert!(offset_of!(HeaderRaw, reserved) == 84);
};

fn unix_nanos_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(thiserror::Error, Debug)]
pub enum ArenaOpenError {
    #[error("arena io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("initial_capacity_slots must be ≥ 1")]
    InitialCapacityZero,

    #[error("file is too small to contain a 4096-byte header (size = {size})")]
    FileTooSmall { size: u64 },

    #[error("invalid magic: expected b\"BARN\", got {0:?}")]
    InvalidMagic([u8; 4]),

    #[error("unsupported format_version {0}")]
    UnsupportedFormatVersion(u32),

    #[error("vector_dim mismatch: expected {expected}, header says {found}")]
    BadVectorDim { expected: u32, found: u32 },

    #[error("slot_size mismatch: expected {expected}, header says {found}")]
    BadSlotSize { expected: u32, found: u32 },

    #[error("header CRC mismatch: expected {expected:#010x}, computed {actual:#010x}")]
    HeaderCrcMismatch { expected: u32, actual: u32 },

    #[error("shard_uuid mismatch: expected {expected:?}, header says {found:?}")]
    ShardUuidMismatch { expected: [u8; 16], found: [u8; 16] },

    #[error(
        "header.slot_count_capacity ({capacity_slots}) is inconsistent with file size \
         ({file_size}); expected file_size = 4096 + capacity_slots * 1600"
    )]
    FileSizeInconsistent { capacity_slots: u64, file_size: u64 },

    #[error("mmap failed: {0}")]
    MmapFailed(std::io::Error),

    #[error("fallocate failed during create: {0}")]
    FallocateFailed(std::io::Error),
}

#[derive(thiserror::Error, Debug)]
pub enum ArenaGrowError {
    #[error("arena cannot shrink (current = {current}, requested = {requested})")]
    ShrinkRequested { current: u64, requested: u64 },

    #[error("fallocate failed: {0}")]
    FallocateFailed(std::io::Error),

    #[error("mremap failed: {0}")]
    MremapFailed(std::io::Error),

    #[error("msync failed: {0}")]
    MsyncFailed(std::io::Error),
}

// ---------------------------------------------------------------------------
// ArenaFile.
// ---------------------------------------------------------------------------

/// Owns one shard's `arena.bin` and the mmap'd view of it.
pub struct ArenaFile {
    file: File,
    base: NonNull<u8>,
    file_size: usize,
    capacity_slots: u64,
    shard_uuid: [u8; 16],
}

// SAFETY: `ArenaFile` exclusively owns its mmap region (no aliased pointers
// leak; we hand out only `&Slot`/`&mut Slot` references whose lifetimes are
// bound to `&self` / `&mut self`). The kernel guarantees the mapping is
// stable across thread boundaries; nothing in `ArenaFile` is thread-local.
unsafe impl Send for ArenaFile {}
// SAFETY: read-only access through `&self` (i.e. `slot()`) returns `&Slot`,
// which is safe to share. Mutations require `&mut self`, which the borrow
// checker prevents from coexisting with `&self`. Any concurrent-writer
// discipline lives one layer up.
unsafe impl Sync for ArenaFile {}

impl ArenaFile {
    /// Open an existing arena at `path`, or create a new one if absent.
    ///
    /// On open: the header is validated. `shard_uuid` must match the
    /// stored one; mismatch → `ShardUuidMismatch`.
    ///
    /// On create: the file is `fallocate`'d to `4096 + initial_capacity_slots
    /// * 1600` bytes, the header is initialized, the header page is
    /// `msync`'d, and madvise hints are applied.
    pub fn open(
        path: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        initial_capacity_slots: u64,
    ) -> Result<Self, ArenaOpenError> {
        if initial_capacity_slots == 0 {
            return Err(ArenaOpenError::InitialCapacityZero);
        }

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let metadata = file.metadata()?;
        if metadata.len() == 0 {
            Self::create_new(file, shard_uuid, initial_capacity_slots)
        } else {
            Self::open_existing(file, shard_uuid)
        }
    }

    fn create_new(
        file: File,
        shard_uuid: [u8; 16],
        initial_capacity_slots: u64,
    ) -> Result<Self, ArenaOpenError> {
        let file_size = HEADER_LEN + (initial_capacity_slots as usize) * SLOT_SIZE;
        let fd = file.as_raw_fd();

        // SAFETY: fd is valid (we own `file`). offset=0, len=file_size,
        // mode=0 are all valid arguments. fallocate is the spec's chosen
        // primitive (§05/03 §3); mode 0 grows the file's reported size.
        let rc = unsafe { libc::fallocate(fd, 0, 0, file_size as libc::off_t) };
        if rc != 0 {
            return Err(ArenaOpenError::FallocateFailed(io::Error::last_os_error()));
        }

        let base = unsafe { mmap_rw(fd, file_size)? };
        apply_madvise(base, file_size);

        // Write the header into the mmap region.
        let header = HeaderRaw {
            magic: HEADER_MAGIC,
            format_version: FORMAT_VERSION_V1,
            shard_uuid,
            vector_dim: VECTOR_DIM_V1,
            slot_size: SLOT_SIZE_V1,
            slot_count_capacity: initial_capacity_slots,
            slot_count_in_use: 0,
            embedding_model_fp_active: [0; 16],
            created_at_unix_nanos: unix_nanos_now(),
            last_grow_at_unix_nanos: 0,
            header_crc32c: 0,
            reserved: [0; 4012],
        };

        // SAFETY: `base` points to the start of a `file_size`-byte mmap;
        // we cast the first `HEADER_LEN` bytes to a `&mut HeaderRaw`. The
        // alignment requirement is 8 bytes, satisfied by mmap's page
        // alignment (4096). No other reference into the mmap exists yet.
        unsafe {
            let header_ptr = base.as_ptr() as *mut HeaderRaw;
            *header_ptr = header;
            (*header_ptr).header_crc32c = compute_header_crc(&*header_ptr);
        }

        // Durably persist the header so a crash before the first slot
        // write doesn't leave a corrupt file.
        // SAFETY: header lives at offset 0..HEADER_LEN of the mmap.
        let rc = unsafe { libc::msync(base.as_ptr() as *mut c_void, HEADER_LEN, libc::MS_SYNC) };
        if rc != 0 {
            // We have an mmap but a durability gap. Bubble the error up;
            // the partial file will be cleaned up by the caller (or left
            // for recovery).
            unsafe { libc::munmap(base.as_ptr() as *mut c_void, file_size) };
            return Err(ArenaOpenError::Io(io::Error::last_os_error()));
        }

        Ok(Self {
            file,
            base,
            file_size,
            capacity_slots: initial_capacity_slots,
            shard_uuid,
        })
    }

    fn open_existing(file: File, expected_shard_uuid: [u8; 16]) -> Result<Self, ArenaOpenError> {
        let metadata = file.metadata()?;
        let file_size_u64 = metadata.len();
        if file_size_u64 < HEADER_LEN as u64 {
            return Err(ArenaOpenError::FileTooSmall {
                size: file_size_u64,
            });
        }
        let file_size = usize::try_from(file_size_u64).map_err(|_| {
            ArenaOpenError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "file size overflows usize",
            ))
        })?;

        let fd = file.as_raw_fd();
        let base = unsafe { mmap_rw(fd, file_size)? };
        apply_madvise(base, file_size);

        // SAFETY: header lives at offset 0..HEADER_LEN of an mmap of length
        // file_size ≥ HEADER_LEN. The cast to `*const HeaderRaw` is sound
        // because the byte layout matches and alignment (8) is satisfied.
        let header_view = unsafe { &*(base.as_ptr() as *const HeaderRaw) };

        // Snapshot every header field we'll inspect *before* taking any
        // path that might munmap. After munmap, `header_view` is dangling,
        // so we can't reference it from the error tuple — that would be
        // use-after-free.
        let magic = header_view.magic;
        let format_version = header_view.format_version;
        let vector_dim = header_view.vector_dim;
        let slot_size = header_view.slot_size;
        let stored_crc = header_view.header_crc32c;
        let actual_crc = compute_header_crc(header_view);
        let header_shard_uuid = header_view.shard_uuid;
        let capacity_slots = header_view.slot_count_capacity;

        // Helper to unmap before bailing out.
        let unmap_then = |err: ArenaOpenError| -> ArenaOpenError {
            // SAFETY: base/file_size came from `mmap_rw` above; nothing else
            // references this mapping.
            unsafe { libc::munmap(base.as_ptr() as *mut c_void, file_size) };
            err
        };

        // Spec §05/02 §11 validation order: magic, then CRC, then UUID/
        // version/dim. (CRC before the field-by-field checks so we don't
        // mistake a corrupted UUID for a real mismatch.)

        if magic != HEADER_MAGIC {
            return Err(unmap_then(ArenaOpenError::InvalidMagic(magic)));
        }

        if stored_crc != actual_crc {
            return Err(unmap_then(ArenaOpenError::HeaderCrcMismatch {
                expected: stored_crc,
                actual: actual_crc,
            }));
        }

        if format_version != FORMAT_VERSION_V1 {
            return Err(unmap_then(ArenaOpenError::UnsupportedFormatVersion(
                format_version,
            )));
        }
        if vector_dim != VECTOR_DIM_V1 {
            return Err(unmap_then(ArenaOpenError::BadVectorDim {
                expected: VECTOR_DIM_V1,
                found: vector_dim,
            }));
        }
        if slot_size != SLOT_SIZE_V1 {
            return Err(unmap_then(ArenaOpenError::BadSlotSize {
                expected: SLOT_SIZE_V1,
                found: slot_size,
            }));
        }
        if header_shard_uuid != expected_shard_uuid {
            return Err(unmap_then(ArenaOpenError::ShardUuidMismatch {
                expected: expected_shard_uuid,
                found: header_shard_uuid,
            }));
        }

        let expected_file_size = HEADER_LEN as u64 + capacity_slots * SLOT_SIZE_V1 as u64;
        if file_size_u64 != expected_file_size {
            return Err(unmap_then(ArenaOpenError::FileSizeInconsistent {
                capacity_slots,
                file_size: file_size_u64,
            }));
        }

        let shard_uuid = header_shard_uuid;

        Ok(Self {
            file,
            base,
            file_size,
            capacity_slots,
            shard_uuid,
        })
    }

    /// Number of slots the arena can address (header's `slot_count_capacity`).
    #[must_use]
    pub fn capacity_slots(&self) -> u64 {
        self.capacity_slots
    }

    /// The shard UUID stored in the header.
    #[must_use]
    pub fn shard_uuid(&self) -> [u8; 16] {
        self.shard_uuid
    }

    /// `msync(MS_SYNC)` the entire mmap region.
    ///
    /// Blocks until all dirty pages in `[0, file_size)` are durable. Called
    /// by the checkpoint writer (spec §05/09 §3 step 3) to ensure every
    /// arena write made before the checkpoint reaches stable storage
    /// before `CHECKPOINT_END` is appended to the WAL.
    ///
    /// `&self` rather than `&mut self`: the syscall doesn't need exclusive
    /// access (the kernel handles synchronization), and spec §05/09 §3
    /// step 2's "no in-flight writes during checkpoint" guarantee is
    /// enforced at the caller layer (`&mut Wal` for the surrounding
    /// `CHECKPOINT_BEGIN`/`END` appends).
    pub fn msync_all(&self) -> std::io::Result<()> {
        #[cfg(test)]
        MSYNC_ALL_CALLS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        // SAFETY: `base` is a valid mmap returned by `mmap_rw`, of length
        // `file_size`. `MS_SYNC` is the spec-prescribed flag (§05/09 §3
        // step 3 + §05/03 §13). No concurrent unmap is possible because
        // `Drop` requires owned `self`.
        let rc = unsafe {
            libc::msync(
                self.base.as_ptr() as *mut c_void,
                self.file_size,
                libc::MS_SYNC,
            )
        };
        if rc != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    /// Read a slot.
    ///
    /// # Panics
    /// Panics if `idx >= capacity_slots()`.
    #[must_use]
    pub fn slot(&self, idx: u64) -> &Slot {
        assert!(
            idx < self.capacity_slots,
            "slot index {idx} out of range (capacity = {})",
            self.capacity_slots
        );
        let off = HEADER_LEN + (idx as usize) * SLOT_SIZE;
        // SAFETY: idx is bounds-checked, so off + SLOT_SIZE ≤ file_size.
        // SLOT_SIZE (1600) and SLOT_ALIGN (64) are the layout values
        // matching the file format. The pointer at this offset is 64-byte
        // aligned because HEADER_LEN (4096) and SLOT_SIZE (1600) are both
        // multiples of 64.
        unsafe {
            let ptr = self.base.as_ptr().add(off) as *const Slot;
            &*ptr
        }
    }

    /// Mutable slot access. Single-writer per shard is enforced by `&mut self`.
    ///
    /// # Panics
    /// Panics if `idx >= capacity_slots()`.
    pub fn slot_mut(&mut self, idx: u64) -> &mut Slot {
        assert!(
            idx < self.capacity_slots,
            "slot index {idx} out of range (capacity = {})",
            self.capacity_slots
        );
        let off = HEADER_LEN + (idx as usize) * SLOT_SIZE;
        // SAFETY: idx is bounds-checked, so off + SLOT_SIZE ≤ file_size.
        // We hold `&mut self`, so no other reference into the mmap exists.
        unsafe {
            let ptr = self.base.as_ptr().add(off) as *mut Slot;
            &mut *ptr
        }
    }

    /// Grow the arena to (at least) `new_capacity_slots`.
    ///
    /// No-op if `new_capacity_slots ≤ current`. Returns `ShrinkRequested`
    /// if asked to shrink — v1 doesn't support that path (spec §05/03 §8).
    ///
    /// On success: file extended via `fallocate`, mapping resized via
    /// `mremap(MREMAP_MAYMOVE)`, header's `slot_count_capacity` and
    /// `last_grow_at_unix_nanos` updated, header CRC refreshed, header page
    /// `msync`'d so the new capacity survives a crash.
    pub fn grow_to(&mut self, new_capacity_slots: u64) -> Result<(), ArenaGrowError> {
        if new_capacity_slots == self.capacity_slots {
            return Ok(());
        }
        if new_capacity_slots < self.capacity_slots {
            return Err(ArenaGrowError::ShrinkRequested {
                current: self.capacity_slots,
                requested: new_capacity_slots,
            });
        }

        let new_file_size = HEADER_LEN + (new_capacity_slots as usize) * SLOT_SIZE;
        let fd = self.file.as_raw_fd();

        // 1. Extend file via fallocate (spec §05/03 §3).
        // SAFETY: fd is valid; offset=0, len=new_file_size are non-negative.
        let rc = unsafe { libc::fallocate(fd, 0, 0, new_file_size as libc::off_t) };
        if rc != 0 {
            return Err(ArenaGrowError::FallocateFailed(io::Error::last_os_error()));
        }

        // 2. mremap (spec §05/03 §4).
        // SAFETY: self.base is a valid mmap returned by mmap_rw; old length
        // is self.file_size; new length is new_file_size > old length.
        // MREMAP_MAYMOVE allows the kernel to relocate if needed.
        let raw = unsafe {
            libc::mremap(
                self.base.as_ptr() as *mut c_void,
                self.file_size,
                new_file_size,
                libc::MREMAP_MAYMOVE,
            )
        };
        if raw == libc::MAP_FAILED {
            return Err(ArenaGrowError::MremapFailed(io::Error::last_os_error()));
        }
        // SAFETY: mremap returned non-MAP_FAILED, so raw is a valid pointer.
        let new_base =
            NonNull::new(raw.cast()).expect("mremap returned non-null on successful return");
        self.base = new_base;
        self.file_size = new_file_size;
        self.capacity_slots = new_capacity_slots;

        // 3. Update the header (capacity, last_grow_at, CRC).
        // SAFETY: header lives at offset 0..HEADER_LEN; we hold &mut self.
        unsafe {
            let header_ptr = self.base.as_ptr() as *mut HeaderRaw;
            (*header_ptr).slot_count_capacity = new_capacity_slots;
            (*header_ptr).last_grow_at_unix_nanos = unix_nanos_now();
            (*header_ptr).header_crc32c = compute_header_crc(&*header_ptr);
        }

        // 4. Re-apply madvise hints (the new region's pages don't inherit
        // them on every kernel). Errors are non-fatal.
        apply_madvise(self.base, self.file_size);

        // 5. msync the header page (spec §05/03 §13).
        // SAFETY: header lives at offset 0..HEADER_LEN of the new mmap.
        let rc =
            unsafe { libc::msync(self.base.as_ptr() as *mut c_void, HEADER_LEN, libc::MS_SYNC) };
        if rc != 0 {
            return Err(ArenaGrowError::MsyncFailed(io::Error::last_os_error()));
        }

        Ok(())
    }
}

impl core::fmt::Debug for ArenaFile {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // Don't print the raw mmap pointer — readers shouldn't depend on its
        // value, and it's not useful for diagnostics.
        f.debug_struct("ArenaFile")
            .field("shard_uuid", &self.shard_uuid)
            .field("capacity_slots", &self.capacity_slots)
            .field("file_size", &self.file_size)
            .finish()
    }
}

impl Drop for ArenaFile {
    fn drop(&mut self) {
        // SAFETY: base/file_size are the values from the most recent open or
        // grow_to. munmap consumes the mapping; the File closes on its own
        // Drop afterwards.
        unsafe { libc::munmap(self.base.as_ptr() as *mut c_void, self.file_size) };
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// `mmap` with `PROT_READ | PROT_WRITE | MAP_SHARED` over the entire file.
///
/// # Safety
/// `fd` must be a valid open file descriptor opened with read+write access,
/// and `len` must equal the file's current size.
unsafe fn mmap_rw(fd: i32, len: usize) -> Result<NonNull<u8>, ArenaOpenError> {
    let raw = libc::mmap(
        core::ptr::null_mut(),
        len,
        libc::PROT_READ | libc::PROT_WRITE,
        libc::MAP_SHARED,
        fd,
        0,
    );
    if raw == libc::MAP_FAILED {
        return Err(ArenaOpenError::MmapFailed(io::Error::last_os_error()));
    }
    Ok(NonNull::new(raw.cast()).expect("mmap returned non-null on successful return"))
}

/// Apply the spec's madvise hints (`MADV_RANDOM`, `MADV_DONTDUMP`) to the
/// arena region. Failures are non-fatal hints; logged at warn.
fn apply_madvise(base: NonNull<u8>, len: usize) {
    // SAFETY: base/len describe a live mmap region.
    unsafe {
        if libc::madvise(base.as_ptr() as *mut c_void, len, libc::MADV_RANDOM) != 0 {
            tracing::warn!(
                error = %io::Error::last_os_error(),
                "madvise(MADV_RANDOM) failed (non-fatal)"
            );
        }
        if libc::madvise(base.as_ptr() as *mut c_void, len, libc::MADV_DONTDUMP) != 0 {
            tracing::warn!(
                error = %io::Error::last_os_error(),
                "madvise(MADV_DONTDUMP) failed (non-fatal)"
            );
        }
    }
}

/// CRC32C over the header's covered region (`bytes[0..80]`). See
/// `HEADER_CRC_COVERAGE_END` and the plan's §3.1 for the rationale on the
/// `[0..80]` choice.
fn compute_header_crc(header: &HeaderRaw) -> u32 {
    let bytes: &[u8] = bytemuck::bytes_of(header);
    crc32c::crc32c(&bytes[0..HEADER_CRC_COVERAGE_END])
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::slot::flags;
    use std::io::{Seek, SeekFrom, Write};

    fn uuid(byte: u8) -> [u8; 16] {
        [byte; 16]
    }

    fn open_fresh(dir: &tempfile::TempDir, capacity: u64) -> ArenaFile {
        ArenaFile::open(dir.path().join("arena.bin"), uuid(1), capacity).expect("open fresh")
    }

    // ---- Open / create paths ---------------------------------------------

    #[test]
    fn create_new_arena_has_correct_size_and_capacity() {
        let dir = tempfile::tempdir().unwrap();
        let arena = open_fresh(&dir, 1024);
        assert_eq!(arena.capacity_slots(), 1024);
        assert_eq!(arena.shard_uuid(), uuid(1));

        let metadata = std::fs::metadata(dir.path().join("arena.bin")).unwrap();
        assert_eq!(metadata.len(), HEADER_LEN as u64 + 1024 * SLOT_SIZE as u64);
    }

    #[test]
    fn reopen_preserves_uuid_and_capacity() {
        let dir = tempfile::tempdir().unwrap();
        {
            let arena = open_fresh(&dir, 32);
            assert_eq!(arena.capacity_slots(), 32);
        }
        let arena = ArenaFile::open(dir.path().join("arena.bin"), uuid(1), 1024).unwrap();
        assert_eq!(arena.capacity_slots(), 32);
        assert_eq!(arena.shard_uuid(), uuid(1));
    }

    #[test]
    fn open_with_wrong_uuid_returns_mismatch() {
        let dir = tempfile::tempdir().unwrap();
        drop(open_fresh(&dir, 16));
        let err = ArenaFile::open(dir.path().join("arena.bin"), uuid(2), 16).unwrap_err();
        match err {
            ArenaOpenError::ShardUuidMismatch { expected, found } => {
                assert_eq!(expected, uuid(2));
                assert_eq!(found, uuid(1));
            }
            other => panic!("expected ShardUuidMismatch, got {other:?}"),
        }
    }

    #[test]
    fn open_with_corrupted_magic_returns_invalid_magic() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        drop(open_fresh(&dir, 8));
        let mut f = OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(0)).unwrap();
        f.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        drop(f);

        let err = ArenaFile::open(&path, uuid(1), 8).unwrap_err();
        assert!(
            matches!(err, ArenaOpenError::InvalidMagic([0xFF, 0xFF, 0xFF, 0xFF])),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_unsupported_format_version_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        drop(open_fresh(&dir, 8));
        // Patch format_version (offset 4) to a future version, then fix
        // CRC so we hit UnsupportedFormatVersion (not HeaderCrcMismatch).
        patch_field_and_refresh_crc(&path, 4, &(99u32).to_le_bytes());

        let err = ArenaFile::open(&path, uuid(1), 8).unwrap_err();
        assert!(
            matches!(err, ArenaOpenError::UnsupportedFormatVersion(99)),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_wrong_vector_dim_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        drop(open_fresh(&dir, 8));
        patch_field_and_refresh_crc(&path, 24, &(512u32).to_le_bytes());

        let err = ArenaFile::open(&path, uuid(1), 8).unwrap_err();
        assert!(
            matches!(err, ArenaOpenError::BadVectorDim { found: 512, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_wrong_slot_size_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        drop(open_fresh(&dir, 8));
        patch_field_and_refresh_crc(&path, 28, &(1664u32).to_le_bytes());

        let err = ArenaFile::open(&path, uuid(1), 8).unwrap_err();
        assert!(
            matches!(err, ArenaOpenError::BadSlotSize { found: 1664, .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn open_with_corrupted_crc_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        drop(open_fresh(&dir, 8));
        // Flip the stored CRC without recomputing.
        let mut f = OpenOptions::new().write(true).open(&path).unwrap();
        f.seek(SeekFrom::Start(80)).unwrap();
        f.write_all(&[0xFF, 0xFF, 0xFF, 0xFF]).unwrap();
        drop(f);

        let err = ArenaFile::open(&path, uuid(1), 8).unwrap_err();
        assert!(
            matches!(err, ArenaOpenError::HeaderCrcMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn initial_capacity_zero_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = ArenaFile::open(dir.path().join("arena.bin"), uuid(1), 0).unwrap_err();
        assert!(
            matches!(err, ArenaOpenError::InitialCapacityZero),
            "got {err:?}"
        );
    }

    // ---- Slot read/write -------------------------------------------------

    #[test]
    fn fresh_slots_are_all_free() {
        let dir = tempfile::tempdir().unwrap();
        let arena = open_fresh(&dir, 16);
        for idx in 0..16 {
            let s = arena.slot(idx);
            assert!(!s.is_occupied(), "slot {idx} should be free");
        }
    }

    #[test]
    fn write_then_reopen_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        {
            let mut arena = ArenaFile::open(&path, uuid(7), 16).expect("create");
            let s = arena.slot_mut(3);
            s.metadata.slot_version = 42;
            s.metadata.flags = flags::OCCUPIED;
            s.metadata.created_at_unix_nanos = 1_700_000_000_000_000_000;
            s.metadata.last_modified_at_unix_nanos = 1_700_000_000_000_000_001;
            s.metadata.embedding_model_fp_short = [0xAB; 16];
            // Use exact-representable f32 values so round-trip checks
            // can use `==` without floating-point fuzz.
            for (i, x) in s.vector.iter_mut().enumerate() {
                *x = (i as f32) * 0.5;
            }
            s.refresh_crc();
        }
        // Reopen; verify the slot survived across the unmap+remap.
        let arena = ArenaFile::open(&path, uuid(7), 16).expect("reopen");
        let s = arena.slot(3);
        assert!(s.is_valid(), "CRC must verify after reopen");
        assert!(s.is_occupied());
        assert_eq!(s.metadata.slot_version, 42);
        assert_eq!(s.vector[10], 5.0);
        assert_eq!(s.vector[100], 50.0);
    }

    #[test]
    fn writes_to_disjoint_slots_do_not_alias() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 16);
        arena.slot_mut(0).metadata.slot_version = 1;
        arena.slot_mut(7).metadata.slot_version = 7;
        arena.slot_mut(15).metadata.slot_version = 15;
        assert_eq!(arena.slot(0).metadata.slot_version, 1);
        assert_eq!(arena.slot(7).metadata.slot_version, 7);
        assert_eq!(arena.slot(15).metadata.slot_version, 15);
        assert_eq!(arena.slot(1).metadata.slot_version, 0);
    }

    #[test]
    #[should_panic(expected = "slot index 16 out of range (capacity = 16)")]
    fn out_of_range_slot_panics() {
        let dir = tempfile::tempdir().unwrap();
        let arena = open_fresh(&dir, 16);
        let _ = arena.slot(16);
    }

    // ---- Grow ------------------------------------------------------------

    #[test]
    fn grow_to_doubles_capacity_and_file_size() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        let mut arena = ArenaFile::open(&path, uuid(1), 16).unwrap();
        arena.grow_to(32).unwrap();
        assert_eq!(arena.capacity_slots(), 32);
        let metadata = std::fs::metadata(&path).unwrap();
        assert_eq!(metadata.len(), HEADER_LEN as u64 + 32 * SLOT_SIZE as u64);
    }

    #[test]
    fn grow_to_same_capacity_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 16);
        arena.grow_to(16).unwrap();
        assert_eq!(arena.capacity_slots(), 16);
    }

    #[test]
    fn grow_to_smaller_returns_shrink_error() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 16);
        let err = arena.grow_to(8).unwrap_err();
        assert!(
            matches!(
                err,
                ArenaGrowError::ShrinkRequested {
                    current: 16,
                    requested: 8
                }
            ),
            "got {err:?}"
        );
        assert_eq!(arena.capacity_slots(), 16, "shrink request must not mutate");
    }

    #[test]
    fn grow_preserves_existing_slot_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        let mut arena = ArenaFile::open(&path, uuid(2), 16).unwrap();
        // Write into slot 5, refresh CRC.
        {
            let s = arena.slot_mut(5);
            s.metadata.slot_version = 999;
            s.metadata.flags = flags::OCCUPIED;
            s.refresh_crc();
        }
        arena.grow_to(64).unwrap();
        let s = arena.slot(5);
        assert!(s.is_valid());
        assert_eq!(s.metadata.slot_version, 999);
    }

    #[test]
    fn grow_zeroes_new_slots() {
        let dir = tempfile::tempdir().unwrap();
        let mut arena = open_fresh(&dir, 16);
        arena.grow_to(32).unwrap();
        for idx in 16..32 {
            let s = arena.slot(idx);
            assert!(!s.is_occupied(), "new slot {idx} must be free");
            assert_eq!(s.metadata.slot_version, 0);
        }
    }

    #[test]
    fn grow_persists_capacity_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        {
            let mut arena = ArenaFile::open(&path, uuid(3), 16).unwrap();
            arena.grow_to(64).unwrap();
        }
        let arena = ArenaFile::open(&path, uuid(3), 16).unwrap();
        assert_eq!(arena.capacity_slots(), 64);
    }

    // ---- Concurrency shape -----------------------------------------------

    #[test]
    fn two_slot_refs_coexist_through_shared_self() {
        // The arena's `slot()` API takes `&self`; this test compiling is the
        // proof that two `&Slot` references through `&self` can coexist (the
        // "concurrent reads of disjoint slots" check from the phase doc).
        fn check(arena: &ArenaFile) -> (&Slot, &Slot) {
            (arena.slot(0), arena.slot(1))
        }
        let dir = tempfile::tempdir().unwrap();
        let arena = open_fresh(&dir, 4);
        let (s0, s1) = check(&arena);
        let _ = (s0.is_occupied(), s1.is_occupied());
    }

    // ---- Drop / reopen smoke --------------------------------------------

    #[test]
    fn drop_then_open_same_path_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("arena.bin");
        drop(ArenaFile::open(&path, uuid(9), 4).unwrap());
        let arena = ArenaFile::open(&path, uuid(9), 4).unwrap();
        assert_eq!(arena.capacity_slots(), 4);
    }

    // ---- Test helpers ---------------------------------------------------

    fn patch_field_and_refresh_crc(path: &std::path::Path, offset: u64, value: &[u8]) {
        // Read the full header, patch the field, recompute the CRC, write back.
        let mut bytes = std::fs::read(path).unwrap();
        bytes[offset as usize..offset as usize + value.len()].copy_from_slice(value);
        let crc = crc32c::crc32c(&bytes[0..HEADER_CRC_COVERAGE_END]);
        bytes[80..84].copy_from_slice(&crc.to_le_bytes());
        std::fs::write(path, &bytes).unwrap();
    }
}
