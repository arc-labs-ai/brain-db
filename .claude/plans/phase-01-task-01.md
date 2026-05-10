# Plan: Phase 1 — Task 1.1, Frame Header Type (retrospective)

**Status:** implemented (commit `6732d5f`)
**Date drafted:** 2026-05-10 (retrospective)
**Author:** Claude (autonomous)

---

## 1. Scope

Pin the wire-format constants and define the fixed 32-byte `Header` struct with CRC32C-protected fields. Provide `Header::new` (constructive, seals the header CRC) and `Header::validate` (read-side, checks magic / version / reserved zeroness / payload-length bound / header CRC).

**Out of scope:**

- Payload encoding (Task 1.4 owns `Frame`).
- Full `ProtocolError` taxonomy (Task 1.6); only the variants needed by `validate` are introduced here.
- Opcode interpretation (Task 1.3); `Header::opcode` is a raw `u8`.

## 2. Spec references

- `spec/03_wire_protocol/03_frame_header.md` — the entire file.
  - §1 — 32-byte byte-level layout.
  - §3.6 — header CRC32C is computed over bytes `0..8 ++ 12..32` (the CRC slot is excluded).
  - §3.7 — payload CRC is independent.
  - §8 — endianness summary: **all multi-byte integers are big-endian**.

Binding constraints:

- Magic = `b"BRN0"`, version = 1, header size = 32, max payload = `(1<<24)-1`.
- BE byte order for `flags`, `header_crc32c`, `stream_id`, `payload_crc32c`; BE u24 for `payload_len`.
- Reserved bytes (`reserved_a` at offset 19, `reserved_b` at offsets 24..32) MUST be zero on the wire.

**Drift correction:** the Phase 1 doc's "Pitfalls" said little-endian. The spec §03/03 §1 and §8 explicitly say big-endian. Implementation followed spec; phase doc was footnoted in commit `6732d5f`.

## 3. External validation

Not applicable — header layout is fully spec-driven. The only external check was confirming `crc32c::crc32c(b"123456789") == 0xE306_9283` (RFC 3720 vector) which validated the CRC32C crate (used inline; promoted to wrappers in Task 1.2).

## 4. Architecture sketch

```text
crates/brain-protocol/src/
├── lib.rs           re-exports + MAGIC, HEADER_SIZE, MAX_PAYLOAD_BYTES
├── error.rs         (new) ProtocolError minimal subset
└── header.rs        (new) Header struct + impl
```

Key decision: store multi-byte fields as raw big-endian byte arrays so the struct is trivially `bytemuck::Pod` and matches the on-wire layout 1:1.

```rust
#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Pod, Zeroable)]
pub struct Header {
    pub magic:           [u8; 4],
    pub version:         u8,
    pub opcode:          u8,
    pub flags:           [u8; 2],   // BE u16
    pub header_crc32c:   [u8; 4],   // BE u32
    pub stream_id:       [u8; 4],   // BE u32
    pub payload_len:     [u8; 3],   // BE u24
    pub reserved_a:      u8,        // must be zero
    pub payload_crc32c:  [u8; 4],   // BE u32
    pub reserved_b:      [u8; 8],   // must be zero
}
```

Compile-time assertions pin `size_of::<Header>() == 32` and `align_of::<Header>() == 1`.

`PartialEq` / `Eq` are implemented by hand against `bytemuck::cast_ref::<_, [u8; 32]>` because `repr(C, packed)` blocks the auto-derive.

## 5. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** byte-array fields + manual encode/decode of native ints. | ✓ Pod-derive works, no padding holes, layout matches wire. |
| Native int fields (`u16`, `u32`, `u64`) with `repr(C, packed)`. | rejected — borrow rules on packed structs make field access awkward; Pod still works but accessors require `addr_of` or copies. Byte-array approach is strictly simpler. |
| Separate `decode` / `encode` methods returning `[u8; 32]` instead of `bytemuck::cast`. | rejected — bytemuck cast is zero-cost and the `Pod` derive already encodes the safety story. |

## 6. Risks / open questions

- **Phase-doc endianness drift** — corrected in this commit; future spec readers should always defer to spec §8.
- **`ProtocolError` is a stub here** — variants are `BadMagic`, `BadVersion`, `BadHeaderCrc`, `OversizePayload`, `ReservedFieldNonZero`. Task 1.6 expands it.
- **No `Default` impl on `Header`** — derive on a `repr(C, packed)` struct emits warnings on some Rust versions; the compile-time `Zeroable` impl is enough for our needs.

## 7. Test plan

Mapped to phase-doc Done-when:

- **Module compiles and tests pass.** ← Build + 12-test suite.
- **bytemuck::Pod derive works (no padding holes).** ← `header_has_correct_size` + compile-time `size_of` assertion.
- **Header::new computes a CRC that validate accepts.** ← `new_then_validate_passes`.

Additional tests added:
- `header_has_correct_alignment` (alignment 1).
- `payload_length_round_trips` (24-bit field encodes/decodes).
- `payload_length_at_24bit_max_round_trips`.
- `validate_rejects_bad_magic`.
- `validate_rejects_bad_version`.
- `validate_rejects_corrupted_crc`.
- `validate_rejects_nonzero_reserved_a`.
- `validate_rejects_nonzero_reserved_b`.
- `pod_roundtrip_via_byte_cast` (cast `Header` → `[u8; 32]` → `Header`).

12 tests, all pass.

## 8. Commit shape

Single commit:

> `6732d5f  1.1: implement frame Header type with CRC validation`

Plus the prerequisite fmt fixup commit `1bbed33` for pre-existing scaffold drift.

## 9. Lessons / handoff

- The `repr(C, packed)` + byte-array fields pattern is the cleanest path to a `bytemuck::Pod` wire struct. Reuse for any future fixed-layout headers.
- Spec wins over phase doc when the two disagree — the docs/phases/ files are guides written by humans and can drift. Verify against `spec/` before locking a design.
- `compute_header_crc` (private fn here) was promoted to a public `Header::seal` in Task 1.4 once `Frame::encode` needed to recompute the CRC after mutating the payload.
