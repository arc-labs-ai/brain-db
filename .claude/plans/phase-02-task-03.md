# Phase 2 — Task 2.3: Arena slot byte layout

**Classification:** moderate (POD layout + CRC + bit-flag helpers; one ambiguity in spec to confirm).

**Spec:** `spec/05_storage_arena_wal/02_arena_layout.md` §§3–4. Cross-checked: `12_open_questions.md` (no relevant entry).

## 1. Scope

This task produces the in-memory shape of one arena slot — exactly 1600 bytes, 64-byte aligned, `bytemuck::Pod`. It includes:

- The `Slot` and `SlotMeta` POD structs.
- Bit-flag definitions (occupied / tombstoned / pending-write / hard-forgotten) per spec §3.2.
- `compute_crc`, `refresh_crc`, `is_valid` over the spec-defined coverage.
- Convenience flag accessor methods.

**Out of scope** (later sub-tasks): the arena file header (4096-byte preamble), file open / mmap / grow (sub-task 2.4), the slot allocator and version bumping (sub-task 2.5), HNSW ↔ slot wiring, the salience and last_modified_at write paths.

## 2. Spec quotes that bind the design

> § 3 (slot total): "1600 bytes total"  
> § 3.1 (vector): "384 f32 values, little-endian, contiguous. Element 0 is at byte offset 0 within the slot; element 383 is at byte offset 1532."  
> § 3.2 (metadata table):
>
> | Offset within metadata | Size | Field |
> |---|---|---|
> | 0 | 4 | slot_version (u32 LE) |
> | 4 | 4 | flags (u32 LE) |
> | 8 | 16 | embedding_model_fp_short (`[u8; 16]`) |
> | 24 | 8 | created_at (u64 LE, unix nanoseconds) |
> | 32 | 8 | last_modified_at (u64 LE) |
> | 40 | 4 | metadata_crc32c (u32 LE) |
> | 44 | 20 | reserved (zero) |
>
> § 3.2 (flags layout): bit 0 occupied, bit 1 tombstoned, bit 2 pending-write, bit 3 hard-forgotten, bits 4–31 reserved.
>
> § 4 (alignment): "Slot size 1600 is a multiple of 64 (cache line size). Slot offsets are also multiples of 64 because 4096 is, and 1600 is. So slots are naturally cache-line-aligned."
>
> § 6 (vector): "Each f32 is 4 bytes, little-endian IEEE 754. NaN, ±Inf, and subnormals are technically representable; the substrate validates that vectors contain only finite values."

## 3. Spec ambiguities surfaced

### 3.1 CRC coverage — likely typo

Spec §3.2 says: *"`metadata_crc32c` … computed over slot metadata bytes [0..36] and the vector bytes"*.

Byte 36 is in the middle of `last_modified_at` (which spans bytes 32–39). The most defensible interpretation is `[0..40]` — every metadata field before the CRC itself. That covers `slot_version + flags + embedding_model_fp_short + created_at + last_modified_at`.

Alternatives considered:
- **Verbatim `[0..36]`**: half the `last_modified_at` field uncovered. Surely wrong.
- **`[0..32]`** (excluding `last_modified_at` so it can be updated without recomputing CRC): defensible, but spec body explicitly says "Computing this CRC on every read would slow the hot path; the substrate computes it only during periodic scrubbing", which contradicts the optimization motive. So no.
- **`[0..40]`** (proposed): natural, consistent with the rest of the spec (CRC excludes only itself + reserved).

**Plan: implement `[0..40]`. Surfacing here for confirmation before code.**

### 3.2 Phase doc disagrees with spec on SlotMeta contents

`docs/phases/phase-02-storage.md` § Task 2.3 lists `SlotMeta` as carrying `agent_id (16B), context_id (16B), kind (1B), salience (4B), flags (1B)`. Spec §3.2 — the authoritative source per AUTONOMY §2 — has none of those. Agent/context/kind/salience live in the metadata store (redb, sub-task 3.x), not the arena.

**Plan: follow the spec. Update the phase doc's Task 2.3 description in the same commit so future readers aren't misled.**

### 3.3 Phase doc's `padding: [u8; N], crc: u32` is wrong

Phase doc sketch shows `Slot { vector, metadata, padding, crc }`. Spec puts `metadata_crc32c` *inside* `SlotMeta` at metadata-offset 40 (= slot-offset 1576). There is no trailing padding+crc field on the slot.

**Plan: follow the spec.**

## 4. Architecture

### 4.1 The two POD structs

```rust
// crates/brain-storage/src/arena/slot.rs

pub const VECTOR_DIM: usize = 384;
pub const VECTOR_BYTES: usize = VECTOR_DIM * 4;            // 1536
pub const META_BYTES: usize = 64;
pub const SLOT_SIZE: usize = VECTOR_BYTES + META_BYTES;    // 1600
pub const SLOT_ALIGN: usize = 64;
pub const META_OFFSET_IN_SLOT: usize = VECTOR_BYTES;       // 1536

/// Bytes of metadata covered by `metadata_crc32c`.
/// Spec §3.2 prints "[0..36]" but byte 36 splits `last_modified_at`;
/// we cover `[0..40]` (every metadata field before the CRC). See plan.
pub const META_CRC_COVERAGE_END: usize = 40;

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

#[repr(C, align(64))]
#[derive(Clone, Copy)]
pub struct Slot {
    pub vector: [f32; VECTOR_DIM],
    pub metadata: SlotMeta,
}

// SAFETY: both structs are #[repr(C)], contain only Pod fields, and have no
// implicit padding (verified by static asserts below).
unsafe impl bytemuck::Zeroable for SlotMeta {}
unsafe impl bytemuck::Pod for SlotMeta {}
unsafe impl bytemuck::Zeroable for Slot {}
unsafe impl bytemuck::Pod for Slot {}
```

Layout proof (no implicit padding):

- `SlotMeta`: u32@0 + u32@4 + [u8;16]@8 + u64@24 + u64@32 + u32@40 + [u8;20]@44 = 64. Alignment max(4, 8, 1, 8, 8, 4, 1) = 8. Size 64 is a multiple of 8 → no trailing pad.
- `Slot`: [f32; 384]@0 (4-aligned, 1536 bytes) + SlotMeta@1536 (8-aligned; 1536 = 192·8 ✓) = 1600. With `align(64)`, struct alignment is 64. Size 1600 is a multiple of 64 → no trailing pad.

Static asserts will catch any future drift.

### 4.2 Flag bit constants and accessors

```rust
pub mod flags {
    pub const OCCUPIED:       u32 = 1 << 0;
    pub const TOMBSTONED:     u32 = 1 << 1;
    pub const PENDING_WRITE:  u32 = 1 << 2;
    pub const HARD_FORGOTTEN: u32 = 1 << 3;
    pub const RESERVED_MASK:  u32 = !(OCCUPIED | TOMBSTONED | PENDING_WRITE | HARD_FORGOTTEN);
}

impl Slot {
    pub fn zeroed() -> Self;             // via bytemuck::Zeroable
    pub fn is_occupied(&self) -> bool;
    pub fn is_tombstoned(&self) -> bool;
    pub fn is_pending_write(&self) -> bool;
    pub fn is_hard_forgotten(&self) -> bool;
    pub fn set_flag(&mut self, mask: u32, on: bool);
    pub fn compute_crc(&self) -> u32;    // CRC32C over vector + meta[0..40]
    pub fn refresh_crc(&mut self);       // store compute_crc() into metadata_crc32c
    pub fn is_valid(&self) -> bool;      // metadata_crc32c == compute_crc()
}
```

`set_flag(mask, on)` is a single helper rather than per-bit setters because we'll grow more flags over time and the caller already names the bit.

### 4.3 CRC computation

```rust
fn compute_crc(&self) -> u32 {
    let bytes: &[u8; SLOT_SIZE] = bytemuck::bytes_of(self).try_into().unwrap();
    let mut hasher = crc32c::Crc32cHasher::default();
    hasher.write(&bytes[0..VECTOR_BYTES]);                                      // vector
    hasher.write(&bytes[META_OFFSET_IN_SLOT..META_OFFSET_IN_SLOT + META_CRC_COVERAGE_END]); // meta[0..40]
    hasher.finish() as u32
}
```

(Or simpler: two `crc32c::crc32c_append` calls. `crc32c` 0.6 exposes both. Confirm in the implementation.)

### 4.4 Endianness guard

Spec §2 says storage is little-endian. We rely on native-order memory access through bytemuck::Pod. Add a crate-level guard:

```rust
#[cfg(not(target_endian = "little"))]
compile_error!("brain-storage requires a little-endian target (spec §05/02 §2 says storage is LE).");
```

(brain-storage is already Linux-only; production deploys on x86_64 / aarch64-le. The guard is cheap insurance against a hypothetical BE Linux target.)

### 4.5 Public surface

```
crates/brain-storage/src/
  arena/
    mod.rs          // pub mod slot;
    slot.rs         // this task's payload
  lib.rs            // add `pub mod arena;` and the LE guard
```

Re-export top-level:

```rust
// lib.rs
pub mod arena;
```

We don't blanket-re-export `arena::slot::*` from the crate root; callers reach for `brain_storage::arena::slot::Slot` (or `arena::Slot` once we add the alias in 2.4). Avoids polluting the crate root with low-level symbols.

## 5. Trade-offs considered

| Option | Verdict | Why |
|---|---|---|
| **A. POD struct + bytemuck (chosen)** | ✓ | Direct field access; safe casts to/from `&[u8; 1600]` via bytemuck; zero-copy mmap reads; static asserts catch drift. |
| B. `struct Slot { bytes: [u8; 1600] }` + accessor methods | ✗ | Loses field ergonomics; reinvents what bytemuck already provides; would still need the same layout invariants. |
| C. `#[repr(C, packed)]` to dodge alignment concerns | ✗ | Forces unaligned reads; aarch64 traps or slows down. We *want* 64-byte alignment for cache locality (spec §4). |
| D. `#[repr(transparent)]` newtype around `[u8; 1600]` | ✗ | Same loss-of-ergonomics as B; gains nothing over A here. |
| E. Keep the spec's literal `[0..36]` CRC range | ✗ | Splits `last_modified_at`; almost certainly a spec typo. Surfaced in §3.1. |
| F. Skip the LE compile guard | ✗ | Cheap to add; catches a class of silent corruption on BE. |

## 6. Risks

- **Spec ambiguity on CRC range** (§3.1). I'm proceeding with `[0..40]` pending your confirmation. If you'd rather I use a different range, edit the plan or say so.
- **CRC scrub timing.** Spec says "computed only during periodic scrubbing or when a recovery suspects a slot." This task only *defines* `compute_crc` / `is_valid`; the scheduler and the "compute on write" call sites are sub-task 2.5 (allocator) and beyond. Document in a doc-comment.
- **Vector finite/L2-norm validation.** Spec §3.1 requires it. This task does *not* enforce it at the type level — bytemuck::Pod accepts any bit pattern. Validation is a separate concern; we'll add a `Slot::validate_vector()` helper in a later task. Note in doc-comment.
- **`#[repr(C, align(64))]` interaction with `Vec<Slot>`**. The `Vec`'s allocator already respects element alignment. Confirmed by Rust reference; no special handling needed. Mmap'd files are page-aligned (4096) which is also 64-aligned.

## 7. Test plan

### Layout invariants (compile-time + runtime)

- `assert_eq!(size_of::<SlotMeta>(), 64)` (`const_assert!` via static check).
- `assert_eq!(size_of::<Slot>(), SLOT_SIZE)` = 1600.
- `assert_eq!(align_of::<Slot>(), 64)`.
- `assert_eq!(memoffset::offset_of!(Slot, metadata), VECTOR_BYTES)` — confirm `Slot.metadata` starts at byte 1536. (Use core's `core::mem::offset_of!`, stable since 1.77; no new dep.)
- bytemuck::Pod derive compiles → proof of no implicit padding.

### CRC behavior (5 tests)

- Round-trip: `let mut s = Slot::zeroed(); set fields; s.refresh_crc(); assert!(s.is_valid())`.
- Vector corruption: flip a vector byte → `is_valid() == false`.
- Metadata corruption (covered range): flip a byte at offset 1536 (slot_version low byte) → `is_valid() == false`.
- Metadata corruption (uncovered range): flip a byte in `metadata.reserved` → `is_valid() == true` (CRC excludes reserved).
- CRC self-exclusion: change `metadata.metadata_crc32c` to a wrong value, then `compute_crc()` returns the same value as before (CRC excludes itself).

### Flag accessor tests (4 tests)

- Zeroed slot is none of {occupied, tombstoned, pending-write, hard-forgotten}.
- `set_flag(flags::OCCUPIED, true)` flips just bit 0; other bits untouched.
- `set_flag(mask, false)` clears a bit; idempotent.
- `is_*` accessors mirror `flags & MASK != 0`.

### Endianness sanity (1 test)

- Write `slot_version = 0x01020304` via the field; reading bytes 1536..1540 of `bytemuck::bytes_of(&slot)` yields `[0x04, 0x03, 0x02, 0x01]` on a little-endian target. (Doubles as proof we're storing LE, matching the spec.)

### Property test (1, with `proptest`)

- For arbitrary slot bytes outside the CRC-covered region (i.e. byte 1576..1600 of the slot — the CRC field plus reserved), `compute_crc(slot)` is invariant under modifications to that region.

**Total: ~12 tests + 4 static asserts.** Maps to phase doc's "Done when":
- size_of::<Slot> == SLOT_SIZE_BYTES → static assert.
- align_of::<Slot> == 64 → static assert.
- CRC verifies for a roundtripped slot → CRC round-trip test.

## 8. Estimated commit shape

One commit on `feature/brain-storage`, message:

> `feat(brain-storage): arena slot byte layout (sub-task 2.3)`

Body covers:
- the two POD structs and their byte layouts,
- the CRC choice (with the `[0..40]` rationale and pointer to spec §3.2 ambiguity),
- the phase-doc correction (2.3 description was at odds with the spec),
- the LE compile guard,
- test count.

Files touched:
- `crates/brain-storage/src/arena/mod.rs` (new, ~5 lines)
- `crates/brain-storage/src/arena/slot.rs` (new, ~280 lines including tests)
- `crates/brain-storage/src/lib.rs` (add `pub mod arena;` + LE guard)
- `docs/phases/phase-02-storage.md` (correct §Task 2.3 sketch + check the boxes)

Verify gate: `cargo fmt --all -- --check && ./scripts/check-skills.sh && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-storage --all-targets` inside the dev container.

---

PLAN READY: see `.claude/plans/phase-02-task-03.md` — confirm to proceed.
