---
name: rust-unsafe-checker
description: Audit unsafe Rust blocks for SAFETY comments, smallest scope, miri coverage, and soundness. The only crate that may use unsafe in Brain is brain-storage; this skill polices that boundary.
when-to-use: |
  Triggers:
    - User says "review this unsafe block" / "is this UB?" / "audit unsafe"
    - Diff touches files matching crates/brain-storage/**/*.rs that contain
      `unsafe`, raw pointer, transmute, repr(C), MaybeUninit, NonNull, FFI, or extern
    - User asks about soundness, aliasing, or alignment
    - Adding new unsafe outside brain-storage — STOP and surface (AUTONOMY §15)
trigger-files:
  - crates/brain-storage/**/*.rs
spec-refs:
  - spec/05_storage_arena_wal/02_arena_layout.md
license: MIT
source: https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/unsafe-checker
---

# Unsafe Rust Checker

## When unsafe is valid (Brain-specific)

Per CLAUDE.md §7 and AUTONOMY §15:

- **Only `crates/brain-storage`** may contain `unsafe`. Discovering `unsafe` anywhere else is a stop-and-surface event — do not silently fix.
- **Inside brain-storage**, valid uses are: `mmap` of the arena, pointer arithmetic on slot buffers, `MaybeUninit` initialization of slot headers, and `bytemuck::Pod` casts on fixed-layout structs.

| Use Case | Example | Brain location |
|----------|---------|----------------|
| FFI | `libc::open`, `pwritev2(RWF_DSYNC)` | brain-storage WAL |
| Low-level abstractions | Slot allocator, arena slab | brain-storage arena |
| Performance | Hot-path slot CRC over `&[u8; 1600]` | brain-storage arena |

**NOT valid:** escaping the borrow checker, "I know what I'm doing", or "it's faster" without a measurement.

## Required documentation

Every `unsafe` block MUST carry a `// SAFETY:` comment in the smallest possible scope. Every `pub unsafe fn` MUST carry a rustdoc `# Safety` section listing caller invariants.

```rust
// SAFETY: ptr was derived from a Vec<u8> of length >= SLOT_BYTES, and
// the slot header guarantees alignment to 8.
unsafe { ptr::read(ptr as *const SlotHeader) }
```

```rust
/// # Safety
///
/// `idx` must be in `0..self.capacity()`. Caller must hold the writer
/// lock for this shard.
pub unsafe fn write_slot(&self, idx: usize, bytes: &[u8; SLOT_BYTES]) { ... }
```

## Workflow

1. **Locate every `unsafe` keyword** in the diff (`grep -n unsafe`).
2. **Per block:**
   - Smallest scope? If the block contains one safe operation followed by one unsafe, split.
   - `// SAFETY:` comment present and accurate? Reject if missing or boilerplate ("trust me").
   - Invariants hold? Walk the comment's claims against the surrounding code.
3. **Per `pub unsafe fn`:** verify the rustdoc `# Safety` section names every caller obligation.
4. **Miri coverage:** confirm at least one test exercises this code path under miri (CLAUDE.md §10).
5. **Boundary check:** the file must be under `crates/brain-storage/`. Anywhere else → STOP and surface.

## Quick reference

| Operation | Safety requirements |
|-----------|---------------------|
| `*ptr` deref | Valid, aligned, initialized |
| `&*ptr` | + no aliasing violations |
| `transmute` | Same size, valid bit pattern (often replaceable with `bytemuck::cast`) |
| `extern "C"` | Correct signature, ABI |
| `static mut` | Synchronization guaranteed (prefer `AtomicT` or `Mutex` — single-writer-per-shard makes this rare) |
| `impl Send/Sync` | Actually thread-safe (per-shard types are intentionally `!Send` — see CLAUDE.md §9) |

## Common errors

| Error | Fix |
|-------|-----|
| Null pointer deref | Check for null before deref; prefer `NonNull<T>` |
| Use after free | Ensure lifetime validity; tie to a borrow |
| Data race | Add proper synchronization or rely on single-writer-per-shard discipline |
| Alignment violation | Use `#[repr(C, packed)]` deliberately; verify with compile-time `align_of` |
| Invalid bit pattern | Use `MaybeUninit<T>` |
| Missing SAFETY comment | Reject the change |

## Deprecated → better

| Deprecated | Use instead |
|------------|-------------|
| `mem::uninitialized()` | `MaybeUninit<T>` |
| `mem::zeroed()` for refs | `MaybeUninit<T>` |
| Raw pointer arithmetic | `NonNull<T>`, `ptr::add` |
| `CString::new().unwrap().as_ptr()` | Store `CString` first |
| `static mut` | `AtomicT`, or per-shard owned data |
| Manual `transmute` for plain casts | `bytemuck::cast` |

## Source / Adaptations

- **Source:** [`actionbook/rust-skills@1f4becd`](https://github.com/actionbook/rust-skills/tree/1f4becdcb88d1cbccc1880594479f28891102843/skills/unsafe-checker)
- **License:** MIT
- **Adaptations:**
  - Renamed `unsafe-checker` → `rust-unsafe-checker` (project naming convention).
  - Replaced upstream `globs: ["**/*.rs"]` with project-specific `trigger-files: crates/brain-storage/**/*.rs`; `unsafe` outside that crate is a stop-and-surface per AUTONOMY §15.
  - Dropped the ASCII-art preamble.
  - Added Brain-specific framing in "When unsafe is valid" (CLAUDE.md §7 / §9 references).
  - Added a per-block / per-fn Workflow section (steps Claude executes).
  - Removed the FFI Crates table — Brain doesn't ship FFI bindings; rust-only stack.
