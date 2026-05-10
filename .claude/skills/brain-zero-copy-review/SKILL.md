---
name: brain-zero-copy-review
description: Verify rkyv/bytemuck usage achieves zero copy on hot read paths — cast_slice not Vec::from, check_archived_root then deref, no intermediate to_vec calls. Spec §03/04 + §05/02.
when-to-use: |
  Triggers:
    - Diff in crates/brain-protocol/src/{request,response}.rs (rkyv codecs)
    - Diff that calls bytemuck::cast_slice / cast_ref / cast / pod_read_unaligned
    - Diff that calls rkyv::check_archived_root / from_bytes / Deserialize
    - User says "is this zero-copy?" / "why is this allocating?"
    - Hot-path reads of vector data or rkyv archives
trigger-files:
  - crates/brain-protocol/src/request.rs
  - crates/brain-protocol/src/response.rs
  - crates/brain-protocol/src/rkyv_codec.rs
  - crates/brain-storage/**/*.rs
  - crates/brain-index/**/*.rs
spec-refs:
  - spec/03_wire_protocol/04_payload_encoding.md
  - spec/05_storage_arena_wal/02_arena_layout.md
---

# Zero-Copy Review

## When to use

Hot-path read code that touches rkyv-encoded structured data or raw vector blobs. Brain's design (spec §03/04) is built around two zero-copy mechanisms:

- **rkyv** for structured payloads — `check_archived_root::<T>(&bytes)` returns `&Archived<T>` directly into the buffer; field access is a pointer deref, no decode loop.
- **bytemuck** for raw vectors — `bytemuck::cast_slice::<u8, f32>(&bytes)` reinterprets the byte slice as `&[f32]`; no allocation, no element-by-element decode.

Either tool is wasted if the calling code allocates around it.

## What this enforces

### Read-path contracts

- **rkyv read:** `check_archived_root` then deref. NO `.deserialize(&mut Infallible)` on the hot path — that's an allocation. Deserialize only when the caller needs an owned value (write paths, tests).
- **Vector read:** `bytemuck::cast_slice<u8, f32>(&payload[off..off+len])` on the trailing raw section. NO `Vec::<f32>::from` or `iter().map(f32::from_le_bytes)`.
- **Header read:** `bytemuck::cast::<[u8; 32], Header>(bytes)` for the 32-byte frame header. NO field-by-field byte-fiddling.
- **Slot read:** `bytemuck::cast::<[u8; 1600], Slot>(slot_bytes)` with a CRC verify *before* any field access.

### Anti-patterns to reject

- `.to_vec()` after `cast_slice`. Defeats the cast.
- `String::from(&archived.text[..])` on the hot read path. Use the archived `&str` directly.
- `Vec::<u8>::from(payload)` before passing to `check_archived_root`. The function takes `&[u8]`; pass the borrow.
- `rkyv::from_bytes::<T>(bytes)?` on a read where the caller only needs to *peek* at one field. Use `check_archived_root` and deref instead.

### Write paths (different rules)

Write paths are allowed to allocate; you're producing the bytes. The rule there is:

- Use `to_rkyv_bytes(&value)` (in `crate::rkyv_codec`).
- Don't double-allocate: emit once into the frame's payload, not into a temporary `Vec` then a `clone` into the frame.

## Workflow

1. **Identify the call site.** Hot-path read = anything reachable from `handle_recall`, `handle_encode`'s read-back, slot fetch, ANN query.
2. **Trace the bytes.** Where do they come from (network buffer, mmap'd arena, redb value)? They should flow as `&[u8]` all the way to the cast/check call. Any `.to_vec()` along the way is a smell.
3. **Find the cast.** Confirm it's a `cast_slice` / `cast_ref` / `check_archived_root`, not a `from_bytes` / `Deserialize` / manual byte loop.
4. **Field access.** After the cast, fields are accessed via `&Archived<T>`. The archived strings are `&ArchivedString` (deref to `&str`); archived vecs are `&ArchivedVec<T>` (deref to `&[T]`). No copying.
5. **Lifetime check.** The borrow chain must outlive the access. If the source bytes are a temporary, the cast result lives no longer — surface a structural fix (lift the bytes to a longer-lived owner).

## Common errors → fixes

| Pattern | Why bad | Fix |
|---|---|---|
| `let v: Vec<f32> = bytemuck::cast_slice(&bytes).to_vec();` | Allocation defeats the cast | Drop `.to_vec()`; return `&[f32]` |
| `let archived = rkyv::from_bytes::<T>(&bytes)?;` on read | Allocates a fresh `T` | `let archived = rkyv::check_archived_root::<T>(&bytes)?;` then deref |
| `let s = String::from(&*archived.text);` on read | Allocation per recall | Borrow `&str` from `&archived.text` |
| `let header: Header = bytemuck::pod_read_unaligned(&bytes)` then immediate access | Aligned-1 struct; no need for unaligned read | `let header: Header = bytemuck::cast(bytes_array)` |
| Reading past the rkyv portion before CRC verify | Reading uncertain data | Verify payload CRC32C before any deref |

## Test coverage suggestions

- **Allocation count.** Use `dhat` or `cargo-asm` to confirm hot reads allocate 0 bytes.
- **Bench against a copying baseline.** A naive `to_vec` version should be visibly slower than the zero-copy version. If they're the same, the zero-copy version isn't actually zero-copy.

## Cross-references

- `rust-perf` — broader hot-path discipline.
- `brain-arena-audit` — slot byte layout and bytemuck casts.
- `brain-protocol-version-bump` — wire-format changes.
- spec §03/04 §3 (rkyv), §4 (bytemuck), §5 (full payload format).

## Examples

### Golden — recall result projection

```rust
// Read the rkyv archive in place
let archived = rkyv::check_archived_root::<RecallResponseFrame>(&payload[..rkyv_end])?;
for r in archived.results.iter() {
    let text: &str = &r.text;                                            // borrow, no alloc
    let vector: &[f32] = bytemuck::cast_slice(&payload[r.vector_offset as usize..][..r.vector_dim as usize * 4]);
    sink.push(MemoryProjection { id: r.memory_id.into(), text, vector });
}
```

Zero allocations on the recall path; everything is a borrow into the payload buffer.

### Counter — accidental copy

```rust
let archived = rkyv::from_bytes::<RecallResponseFrame>(&payload[..rkyv_end])?;
                                                                              // ↑ allocation
for r in &archived.results {
    let text: String = r.text.clone();                                        // ↑ another
    let vector: Vec<f32> = bytemuck::cast_slice(...).to_vec();                // ↑ another
}
```

Three allocations per result. Recall returning 100 results allocates 300 times. Defeats the zero-copy design.

## Source / Adaptations

Project-local. Operationalizes spec §03/04.
