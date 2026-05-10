# Phase 2 — Task 2.4: Arena file — open, mmap, grow

**Classification:** moderate-to-heavy. First place in the workspace that touches `unsafe` and syscalls. Touches one new external dep (`libc`). One spec ambiguity to confirm; one architectural decision (memmap2 vs hand-rolled libc) that pins the shape of subsequent tasks.

**Spec:** `spec/05_storage_arena_wal/01_arena_overview.md`, `02_arena_layout.md` §§1–2, §§7–11, `03_arena_growth.md` (full). Cross-checked against `spec/01_system_architecture/05_hardware.md` §1.1 (Linux pinning). `12_open_questions.md` reviewed; no relevant entries.

## 1. Scope

This task delivers `ArenaFile`, the owning handle for one shard's `arena.bin`. Specifically:

- `ArenaFile::open(path, shard_uuid, initial_capacity_slots)` — opens an existing arena or creates a new one. Validates header on open; initializes header on create.
- `ArenaFile::slot(idx) -> &Slot` (immutable).
- `ArenaFile::slot_mut(idx) -> &mut Slot` (single-writer; through `&mut self`).
- `ArenaFile::grow_to(new_capacity_slots)` — extends the file via `fallocate(2)`, remaps via `mremap(2)` with `MREMAP_MAYMOVE`, updates the header's `slot_count_capacity`, and `msync`s the header page.
- `Drop` calls `munmap` (close-on-drop file too).
- Crate-level `madvise(MADV_RANDOM | MADV_DONTDUMP)` per `spec/01/05_hardware.md` §1.1.

**Out of scope** (later sub-tasks):
- The slot allocator and free list (2.5).
- Slot-version bumping on reclaim (2.5).
- Concurrency wrapper around `ArenaFile` (`Arc<ArcSwap<...>>`) — comes when we glue arena + allocator + WAL together for the writer task.
- `slot_count_in_use` upkeep (advisory; written by allocator in 2.5).
- Fault-tolerance during grow (kill-during-grow recovery is a 2.10 concern; this task surfaces failures via `Err`).

## 2. Spec quotes that bind the design

**Header (spec §05/02 §2):**

| Offset | Size | Field | Type |
|---|---|---|---|
| 0 | 4 | magic | "BARN" |
| 4 | 4 | format_version | u32 LE |
| 8 | 16 | shard_uuid | [u8; 16] |
| 24 | 4 | vector_dim | u32 LE (must be 384 for v1) |
| 28 | 4 | slot_size | u32 LE (must be 1600 for v1) |
| 32 | 8 | slot_count_capacity | u64 LE |
| 40 | 8 | slot_count_in_use | u64 LE (advisory) |
| 48 | 16 | embedding_model_fp_active | [u8; 16] |
| 64 | 8 | created_at | u64 LE, unix nanoseconds |
| 72 | 8 | last_grow_at | u64 LE |
| 80 | 4 | header_crc32c | u32 LE |
| 84 | 4012 | reserved | zero |

> §05/02 §2: "header_crc32c is computed over bytes 0–75 (i.e., excluding the CRC field itself and the reserved region)."

**Initial size (spec §05/02 §9):** "`slot_count_capacity = 1024` (default initial capacity). File on disk: 4096 + 1024 × 1600 = 1,642,496 bytes ≈ 1.6 MB, sparse if the filesystem supports it."

**Mmap (spec §05/01 §7):** "`mmap(NULL, file_size, PROT_READ|PROT_WRITE, MAP_SHARED, fd, 0)` … `MAP_SHARED` — writes through the pointer are persisted to the file. We do not use `MAP_PRIVATE`."

**Growth procedure (spec §05/03 §2):**
> 1. Compute the new capacity: `new_capacity = current_capacity × 2`.  
> 2. Compute the new file size: `4096 + new_capacity × 1600`.  
> 3. Extend the file via `fallocate(fd, 0, 0, new_file_size)`.  
> 4. Re-map: either via `mremap` (Linux) or by `mmap`-ing additional pages.  
> 5. Update the header's `slot_count_capacity` field.  
> 6. Sync the header (single 4 KB page) to ensure the new capacity survives a crash.

**`MREMAP_MAYMOVE` (spec §05/03 §4):**
> "`MREMAP_MAYMOVE` lets the kernel relocate the mapping if there's no contiguous space at the old address."

**`msync(MS_SYNC, 4096)` after grow (spec §05/03 §13):**
> "`MS_SYNC` blocks until the page is durably written. This is the only fsync the arena performs in the hot path; other arena writes are asynchronous (the WAL is the durability mechanism)."

**Doubling policy (spec §05/03 §1):** Sequence `1024 → 2048 → … → 268M`. (This task implements `grow_to(N)` as a primitive; the doubling policy is enforced by the *caller*, which lands in 2.5's allocator.)

**Madvise hints (spec §01/05 §1.1):** `MADV_RANDOM`, `MADV_DONTDUMP` — part of the platform pinning.

## 3. Spec ambiguities surfaced

### 3.1 Header CRC coverage — same typo pattern as 2.3

Spec §05/02 §2 says `header_crc32c` is "computed over bytes [0..76]" but `last_grow_at` (u64) spans bytes 72..80. `[0..76]` cuts that field 4 bytes short — exactly the same pattern as the slot CRC's `[0..36]` cutting `last_modified_at`.

Three readings:
- **Verbatim `[0..76]`**: half of `last_grow_at` uncovered. Almost certainly wrong.
- **`[0..72]`** (excludes `last_grow_at` so it can be updated on every grow without touching the CRC): defensible. `last_grow_at` is updated by `grow_to`, which already needs to recompute the CRC anyway, so this gains nothing.
- **`[0..80]`** (every header field before `header_crc32c`): natural, parallel to the slot-CRC fix from 2.3.

**Plan: implement `[0..80]`**, mirroring the 2.3 decision. Surfaced here for confirmation; happy to switch.

### 3.2 Phase doc shape vs spec shape

Phase doc 2.4 prescribes:
```
pub struct ArenaFile { mmap: MmapMut, file: File, capacity_slots: usize }
```
This implies the `memmap2` crate (`MmapMut`). Spec §05/03 §4 explicitly invokes `mremap(2)`, which `memmap2` does *not* expose. To satisfy both, we'd be forced into the spec's "fallback" path (§05/03 §5: `mmap` a fresh region, swap, `munmap` old) instead of the spec's primary `mremap` path.

**Plan: deviate from the phase doc. Hand-roll the mmap region with `libc` so we can use `mremap` directly.** Rationale captured in §4 below; corrected sketch updated in the phase doc as part of this task's commit.

## 4. Architectural decisions

### 4.1 Hand-rolled `MmapRegion` over `memmap2`

| Option | Verdict | Why |
|---|---|---|
| **A. Hand-rolled `libc::mmap` / `mremap` / `munmap` (chosen)** | ✓ | Lets us use `mremap(MREMAP_MAYMOVE)`, the spec's primary growth path. Single owned region we grow in place. One unsafe block per syscall, each with `// SAFETY:`. |
| B. `memmap2::MmapMut` + unmap-and-remap on grow | ✗ | Forces the spec's *fallback* growth path. Brief "no mapping" window during swap. Doesn't actually save unsafe (memmap2 wraps the same syscalls). |
| C. `nix` for everything | ✗ | Adds a heavier dep with broader scope; we only need a handful of syscalls. `libc` raw bindings are well-understood and stable. |
| D. `rustix` | ✗ | Modern, sound, but its `mremap` support is less complete than `libc`'s direct binding (and rustix docs warn the API can change). Conservative choice for a load-bearing crate. |

The blast radius is small — `arena/file.rs` is the only place that uses `unsafe`. The crate already declares it's the only unsafe-bearing crate in the workspace (`lib.rs` doc comment). Hand-rolling fits that contract.

### 4.2 `ArenaFile` shape

```rust
pub struct ArenaFile {
    file: std::fs::File,        // owned; closes on Drop
    base: NonNull<u8>,          // mmap base, never null after open
    file_size: usize,           // 4096 + capacity_slots * SLOT_SIZE
    capacity_slots: usize,      // mirrors the header's slot_count_capacity
}

// SAFETY: the mmap region is owned exclusively by this struct (no aliasing
// pointers leak). We never give out raw pointers to callers; only `&Slot`
// or `&mut Slot` references whose lifetimes are tied to `&self` / `&mut self`.
unsafe impl Send for ArenaFile {}
// We do *not* impl Sync; concurrent reads through `&Arc<ArenaFile>` work
// because slot() takes `&self` and returns `&Slot` (both Sync via shared
// reference rules), but we don't need the struct itself to be Sync — Arc is.
//
// Actually we do want Sync: Arc<T>: Send + Sync requires T: Send + Sync. So:
unsafe impl Sync for ArenaFile {}
// Justification: `&self` access is read-only at the type level. Single-writer
// discipline lives one layer up; this struct exposes &mut self for writes,
// which Sync allows.
```

### 4.3 API surface

```rust
impl ArenaFile {
    /// Open existing or create new. `shard_uuid` is checked against the
    /// header on open and written into the header on create.
    pub fn open(
        path: impl AsRef<Path>,
        shard_uuid: [u8; 16],
        initial_capacity_slots: u64,
    ) -> Result<Self, ArenaOpenError>;

    pub fn capacity_slots(&self) -> u64;
    pub fn shard_uuid(&self) -> [u8; 16];

    /// Read access. `idx < capacity_slots()`; otherwise panics with
    /// `expect("slot index out of range")`. Bounds checks are debug-only
    /// in the hot path? — TBD; default to runtime assert for v1.
    pub fn slot(&self, idx: u64) -> &Slot;
    pub fn slot_mut(&mut self, idx: u64) -> &mut Slot;

    /// Grow to at least `new_capacity_slots`. No-op if already ≥. Performs:
    /// fallocate → mremap → header update → msync(header_page, MS_SYNC).
    pub fn grow_to(&mut self, new_capacity_slots: u64) -> Result<(), ArenaGrowError>;
}

#[derive(thiserror::Error, Debug)]
pub enum ArenaOpenError {
    Io(#[from] std::io::Error),
    InvalidMagic([u8; 4]),
    UnsupportedFormatVersion(u32),
    BadVectorDim { expected: u32, found: u32 },     // v1 = 384
    BadSlotSize  { expected: u32, found: u32 },     // v1 = 1600
    HeaderCrcMismatch { expected: u32, actual: u32 },
    ShardUuidMismatch { expected: [u8; 16], found: [u8; 16] },
    FileSizeInconsistent { capacity_slots: u64, file_size: u64 },
    MmapFailed(std::io::Error),
    InitialCapacityZero,
}

#[derive(thiserror::Error, Debug)]
pub enum ArenaGrowError {
    FallocateFailed(std::io::Error),
    MremapFailed(std::io::Error),
    MsyncFailed(std::io::Error),
    /// Returned for callers asking us to shrink — disallowed in v1
    /// per spec §05/03 §8 ("The arena does not shrink in v1.")
    ShrinkRequested { current: u64, requested: u64 },
}
```

### 4.4 Header init, validation, and CRC

```rust
const HEADER_LEN: usize = 4096;
const MAGIC: [u8; 4] = *b"BARN";
const FORMAT_VERSION_V1: u32 = 1;
const HEADER_CRC_COVERAGE_END: usize = 80;          // see §3.1 above

fn write_header(buf: &mut [u8; HEADER_LEN], shard_uuid, capacity, in_use) { ... }
fn read_and_validate_header(bytes: &[u8; HEADER_LEN]) -> Result<HeaderView> { ... }
fn header_crc(bytes_first_80: &[u8]) -> u32 { crc32c::crc32c(bytes_first_80) }
```

Header is read directly from the mmap region; it lives at file offset 0..4096. No need to `pread` it separately.

### 4.5 Slot pointer arithmetic

```rust
impl ArenaFile {
    fn slot_byte_offset(idx: u64) -> usize {
        HEADER_LEN + (idx as usize) * SLOT_SIZE
    }

    pub fn slot(&self, idx: u64) -> &Slot {
        assert!(idx < self.capacity_slots as u64, "slot {idx} >= capacity");
        let off = Self::slot_byte_offset(idx);
        // SAFETY: idx is bounds-checked. The mmap covers
        // file_size = HEADER_LEN + capacity_slots * SLOT_SIZE, and
        // SLOT_SIZE/SLOT_ALIGN match the file layout. The pointer at this
        // offset is 64-byte aligned because off is a multiple of 64
        // (HEADER_LEN=4096 and SLOT_SIZE=1600 are both multiples of 64).
        // bytemuck::from_bytes asserts size & alignment match Slot's layout.
        unsafe {
            let ptr = self.base.as_ptr().add(off);
            &*(ptr as *const Slot)
        }
    }
}
```

(I might use `bytemuck::from_bytes` instead of `&*(ptr as *const Slot)`; need to confirm bytemuck has a slice-based variant that doesn't require the slice's length to match exactly. Likely use raw cast with the SAFETY comment above; it's the natural form for mmap regions.)

### 4.6 `mremap` flow

```rust
pub fn grow_to(&mut self, new_capacity_slots: u64) -> Result<(), ArenaGrowError> {
    if new_capacity_slots <= self.capacity_slots as u64 {
        return Ok(());      // no-op or noop-shrink (rejected at &mut entry per §4.3 errors)
    }

    let new_file_size = HEADER_LEN + (new_capacity_slots as usize) * SLOT_SIZE;

    // 1. Extend file (sparse on supporting filesystems).
    // SAFETY: fd is valid (we hold the File). offset=0, len=new_file_size are
    // both non-negative. mode=0 is the spec's choice (§05/03 §3).
    let rc = unsafe { libc::fallocate(self.fd(), 0, 0, new_file_size as libc::off_t) };
    if rc != 0 { return Err(ArenaGrowError::FallocateFailed(io::Error::last_os_error())); }

    // 2. mremap with MREMAP_MAYMOVE.
    // SAFETY: self.base is a valid mmap pointer of length self.file_size.
    // new_file_size > self.file_size (checked above).
    let new_addr = unsafe {
        libc::mremap(
            self.base.as_ptr() as *mut c_void,
            self.file_size,
            new_file_size,
            libc::MREMAP_MAYMOVE,
        )
    };
    if new_addr == libc::MAP_FAILED { return Err(ArenaGrowError::MremapFailed(io::Error::last_os_error())); }

    // 3. Commit the new region.
    self.base = NonNull::new(new_addr.cast()).expect("mremap returned non-null on success");
    self.file_size = new_file_size;
    self.capacity_slots = new_capacity_slots as usize;

    // 4. Update header.slot_count_capacity (mmap'd write).
    //    Update header.last_grow_at.
    //    Recompute and store header_crc32c.
    self.write_header_capacity_and_crc();

    // 5. msync the header page.
    // SAFETY: header lives at offset 0..4096; whole-page sync.
    let rc = unsafe { libc::msync(self.base.as_ptr() as *mut c_void, HEADER_LEN, libc::MS_SYNC) };
    if rc != 0 { return Err(ArenaGrowError::MsyncFailed(io::Error::last_os_error())); }

    Ok(())
}
```

Failure semantics: if step 1 (fallocate) fails, no state changed. If step 2 (mremap) fails, the file is larger but the mapping is unchanged — the next open will reconcile via the file size; we don't need to truncate back. If step 5 (msync) fails, the new capacity is in mmap memory but not durable; on a subsequent crash, recovery sees the old capacity per spec §05/03 §13. Caller treats this as a soft warning (the operation didn't lose data; it just may need to redo growth on restart).

### 4.7 Drop

```rust
impl Drop for ArenaFile {
    fn drop(&mut self) {
        // SAFETY: base/file_size are the values from open or the most recent
        // grow_to. munmap consumes the mapping.
        unsafe { libc::munmap(self.base.as_ptr() as *mut c_void, self.file_size) };
        // file closes via std::fs::File's Drop.
    }
}
```

### 4.8 Madvise

After mmap (initial open and after each grow):

```rust
unsafe {
    libc::madvise(self.base.as_ptr() as _, self.file_size, libc::MADV_RANDOM);
    libc::madvise(self.base.as_ptr() as _, self.file_size, libc::MADV_DONTDUMP);
}
```

Failures from `madvise` are non-fatal hints; we log via `tracing` (already a workspace dep) and continue.

## 5. Dependency choice

Adding `libc` as a direct dep of `brain-storage`. Justification:

- Already transitively present in essentially every Linux Rust binary; not a meaningful supply-chain expansion.
- Stable, well-audited, minimal API surface.
- The spec explicitly invokes `libc::mremap`, `libc::msync`, `libc::madvise`, `libc::MS_SYNC`, `libc::MREMAP_MAYMOVE` (§05/03 §§4, 13). Using `libc` directly mirrors the spec.
- Alternative considered: `nix` for slightly safer wrappers. Rejected: `nix` adds a heavier dep, and it tends to evolve faster than `libc` (more breakage risk for a load-bearing crate).
- Alternative considered: `rustix`. Rejected: `mremap` support is less mature; conservatism wins for v1.

Add to `crates/brain-storage/Cargo.toml`:
```toml
libc = "0.2"            # syscalls for mmap/mremap/munmap/fallocate/msync/madvise
tracing = { workspace = true }   # warn-but-continue on madvise failure
```

`tracing` is already in the workspace dep table (per CLAUDE.md §6). I'll add the `libc` workspace pin to the root `Cargo.toml` and reference it via `workspace = true`, mirroring the existing pattern for `crc32c` etc.

## 6. Risks

- **Spec ambiguity on header CRC range** (§3.1). Plan implements `[0..80]`; pending confirmation.
- **mremap pointer movement.** The kernel may move the mapping. We hold `&mut self` for grow, so no concurrent borrows of `&Slot` can outlast the call. Sub-task 2.5 (allocator + writer task) will need to coordinate with the eventual arc-swap layer; not 2.4's problem.
- **Concurrency**: `slot(&self) -> &Slot` makes the borrow checker enforce the no-grow-while-reading invariant. The single-writer-per-shard discipline in sub-task 2.5+ is a separate layer.
- **Page size assumption.** Spec assumes 4 KB pages (Linux x86_64). aarch64 dev container also uses 4 KB by default. Some aarch64 kernels use 16 KB or 64 KB. We don't gate on page size — the header is 4 KB which is page-aligned for any reasonable page size. Add a startup check: `assert!(sysconf(_SC_PAGE_SIZE) <= HEADER_LEN)` so we fail loudly on a hypothetical 8K-page kernel where the header doesn't fully cover one page.
- **fallocate on tmpfs** (used in tests via `tempfile::TempDir`) — tmpfs supports fallocate from kernel 3.5+ (`do_falloc`). Tests will work in the dev container; if they fail in any environment, fall back to manual `set_len`.
- **Atomic writes to mmap'd header.** We write a u64 + a u32 + a u32 (capacity, last_grow_at low/high — wait, u64 is 8 bytes). On x86_64/aarch64 these are atomic at 8-byte alignment. The CRC is the safety net regardless: if a torn write happens, the CRC catches it.
- **f32-tested floats in slot tests round-trip exactly.** Already tested in 2.3.
- **`std::fs::File` ownership.** Holds the fd; closes on drop. Mmap continues to work after `File` is closed (kernel keeps a reference); we keep the `File` around so we can call `fallocate` later (needs the fd).

## 7. Test plan

All tests use `tempfile::TempDir` for the arena path. Run inside the dev container; the LE/Linux compile gates already prevent host runs.

### Open / create (8 tests)

1. `open` on a nonexistent path: creates a new arena, file size = `4096 + 1024 * 1600`, header magic = `"BARN"`, capacity = 1024.
2. Re-`open` of the same path returns same shard_uuid + capacity.
3. `open` with mismatched `shard_uuid` returns `ShardUuidMismatch`.
4. `open` with a corrupted magic byte returns `InvalidMagic([…])`.
5. `open` with a wrong format_version returns `UnsupportedFormatVersion(_)`.
6. `open` with `vector_dim != 384` (manually corrupted) returns `BadVectorDim`.
7. `open` with `slot_size != 1600` returns `BadSlotSize`.
8. `open` with a corrupted CRC field returns `HeaderCrcMismatch`.

### Slot read/write (4 tests)

9. Brand-new arena: every slot is zero (free; flags bit 0 = 0).
10. Write slot 0 via `slot_mut`, set fields, `refresh_crc`, drop arena, reopen, read slot 0, assert `is_valid()` and field round-trip.
11. Write multiple slots (0, 5, 1023) and verify they don't bleed into each other.
12. Out-of-range slot index panics with the expected message.

### Grow (5 tests)

13. `grow_to(2048)` from 1024: capacity becomes 2048, file size matches, header reflects new capacity (re-read after sync), header CRC is valid.
14. `grow_to(N)` for N ≤ current is a no-op (Ok, no fallocate observed).
15. `grow_to(1)` when current is 1024 returns `ShrinkRequested`.
16. After `grow_to`, previously-written slots in the original range still verify `is_valid()`.
17. After `grow_to`, new slots in `[old_capacity, new_capacity)` are zeroed (free).

### Concurrency-shape compile test (1)

18. `fn _compiles(arena: &ArenaFile) -> (&Slot, &Slot) { (arena.slot(0), arena.slot(1)) }` — proves that two `&Slot` references through `&self` coexist. This is the "concurrent reads of disjoint slots" check from the phase doc, satisfied at the type level.

### `unsafe`/`Drop` smoke (1)

19. Open, drop, open the same path again: succeeds (proves munmap doesn't leave the file in a bad state).

### Tests not included (deferred)

- Kill-during-grow recovery: that's sub-task 2.10's territory.
- `madvise` failure path: hard to provoke in a unit test; covered by inspection.
- Cross-process concurrent open: not supported in v1 (single process per shard).

## 8. Estimated commit shape

One commit on `feature/brain-storage`:

> `feat(brain-storage): arena file open/mmap/grow (sub-task 2.4)`

Body:
- Hand-rolled `MmapRegion` + spec rationale (mremap is the spec's primary path).
- Header init/validation; the `[0..80]` CRC choice (parallel to 2.3).
- `grow_to` flow (fallocate → mremap → header update → msync), failure semantics.
- Drop/munmap, madvise hints.
- Phase doc 2.4 corrected (`memmap2::MmapMut` → hand-rolled).
- `libc` added to workspace deps + `crates/brain-storage`.
- Test count.

Files touched:
- `Cargo.toml` (workspace) — add `libc = "0.2"` to `[workspace.dependencies]`.
- `crates/brain-storage/Cargo.toml` — add `libc.workspace = true`, `tracing.workspace = true`.
- `crates/brain-storage/src/arena/mod.rs` — `pub mod file;` + re-export `ArenaFile`.
- `crates/brain-storage/src/arena/file.rs` — new, ~480 lines including tests.
- `docs/phases/phase-02-storage.md` — correct §Task 2.4 sketch + check the boxes after merge.

Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container.

---

PLAN READY: see `.claude/plans/phase-02-task-04.md` — confirm to proceed.
