# Plan: Phase 1 — Task 1.4, Frame Envelope (retrospective)

**Status:** implemented (commit `ef5e3a7`)
**Date drafted:** 2026-05-10 (retrospective)
**Author:** Claude (autonomous)

---

## 1. Scope

Define `Frame { header: Header, payload: Vec<u8> }` and the wire codec: `Frame::encode` (canonical sealing point — recomputes `payload_len`, `payload_crc32c`, `header_crc32c`) and `Frame::decode` / `Frame::decode_with_max` (validate, parse, return `(Self, &[u8] /* tail */)`).

**Out of scope:**

- Payload-content interpretation (rkyv struct decode for request/response bodies — Task 1.7+).
- Multi-payload frame composition (`MPL` flag) — that's a higher-level reassembly concern.
- Property tests — Task 1.5 wraps them around this codec.

## 2. Spec references

- `spec/03_wire_protocol/03_frame_header.md` — header layout (already implemented in 1.1) and `4.1 The reader's algorithm` (sequence of checks: magic → version → reserved → header CRC → payload bytes → payload CRC).
- `spec/03_wire_protocol/04_payload_encoding.md` — payload is opaque to this layer; structured decode happens later. The `Frame` codec doesn't interpret rkyv or vector blobs.
- `spec/03_wire_protocol/10_errors.md` §3.1 — error codes that the codec emits map to `BadMagic`, `BadHeaderCrc`, `BadPayloadCrc`, `OversizePayload`, `BadVersion`, `BadFrame` (truncated input fits here per spec).

Binding constraints:

- §03/03 §3.7: empty payload → `payload_crc32c = 0`. CRC of `&[]` is 0 in CRC32C, so this is automatic.
- §03/03 §1: payload immediately follows the 32-byte header.
- §03/11 §1: validation is deterministic; same bytes always produce the same accept/reject decision. Codec must be panic-free on adversarial input.

## 3. External validation

Not applicable — composes existing pieces (`Header`, `crc::payload_crc`).

## 4. Architecture sketch

```text
brain-protocol/src/frame.rs

pub struct Frame {
    pub header:  Header,
    pub payload: Vec<u8>,
}

impl Frame {
    pub fn new(opcode, flags, stream_id, payload) -> Self;     // wire-ready

    pub fn encode(&self) -> Vec<u8>;                             // canonical seal
    pub fn decode(bytes) -> Result<(Self, &[u8]), ProtocolError>;
    pub fn decode_with_max(bytes, max_payload_bytes)
        -> Result<(Self, &[u8]), ProtocolError>;
}
```

Header gains a `seal(&mut self)` helper that recomputes the header CRC after callers (i.e. `Frame::encode`) have written `payload_len` and `payload_crc32c`. `Header::new` is refactored to call `seal`.

`ProtocolError` adds `BadPayloadCrc` and `Truncated { have, need }` variants.

`Header` gains a hand-written `PartialEq` / `Eq` (byte-wise via `bytemuck::cast_ref`) so `Frame` can derive its own.

## 5. Trade-offs considered

| Alternative | Verdict |
|---|---|
| **Chosen:** `encode` is canonical — recomputes lengths and CRCs from `self.payload`. Caller can mutate `header.opcode`/`flags`/`stream_id` without resealing. | ✓ Eliminates a class of "stale CRC" bugs. |
| `encode` trusts the caller's `header` as-is. | rejected — too easy to ship a frame whose `payload_len` doesn't match `payload.len()`. |
| `decode_with_max` always uses the spec's hard 24-bit cap (no configurable max). | rejected — at handshake time the server may negotiate a smaller `max_payload_size`; decoder needs to enforce it before allocating. |
| `decode` consumes the input slice (returns `(Self, ())`). | rejected — stream readers want to keep reading past one frame; returning the tail is essential. |
| Borrow-rather-than-own the payload (`Frame<'a> { payload: &'a [u8] }`). | considered; deferred — owned `Vec<u8>` is simpler and matches the phase doc; zero-copy variant can come later if benchmarks demand. |

## 6. Risks / open questions

- **Header alignment** — `Header` is align-1 (packed); reading from any byte boundary is safe via `bytemuck::cast`. Verified at compile time (`align_of::<Header>() == 1`).
- **Allocation amplification** — addressed by `decode_with_max` checking `payload_len` against the cap *before* slicing the input. A peer claiming `payload_len = 16 MiB` can't force a 16 MiB allocation; we just reject.
- **Empty payload + `payload_crc32c = 0`** — relies on CRC32C of empty input being 0. Pinned in Task 1.2's tests.

## 7. Test plan

Mapped to phase-doc Done-when:

- **All seven test cases pass:**
  - `encode_then_decode_roundtrip`
  - `decode_rejects_bad_magic`
  - `decode_rejects_bad_version`
  - `decode_rejects_bad_header_crc`
  - `decode_rejects_bad_payload_crc`
  - `decode_rejects_truncated_header` + `decode_rejects_truncated_payload` (split into two for the two failure modes)
  - `decode_rejects_oversize_payload` (uses `decode_with_max`)
- **encode and decode are inverses for valid frames.** ← `encode_then_decode_roundtrip` plus `encode_seals_against_caller_drift`.
- **Errors match the variants in spec §10.** ← all rejection tests pattern-match the `ProtocolError` variant.

Additional:

- `encode_then_decode_empty_payload` — pins spec §3.7 (empty → CRC 0).
- `decode_returns_unconsumed_tail` — stream-reader contract.
- `encode_seals_against_caller_drift` — mutate payload after `Frame::new`; encode reseals.

11 tests total in `frame.rs`.

## 8. Commit shape

Single commit:

> `ef5e3a7  1.4: add Frame envelope with encode/decode`

Total diff: ~308 insertions across `error.rs`, `frame.rs` (new), `header.rs`, `lib.rs`, phase doc.

## 9. Lessons / handoff

- The "canonical seal at `encode`" pattern shifts the stale-CRC failure mode from a runtime bug to an impossibility — copied verbatim into `RequestBody::encode` (Task 1.7) and `ResponseBody::encode` (Task 1.8).
- `decode_with_max` will be called by the connection handler in Phase 9 with `max_payload_size` from `WelcomePayload.server_features`. The hook is in place; wiring is later.
- `Header::seal` — the public version of `Header::compute_header_crc` from Task 1.1 — is the right primitive to expose. Future header mutators (e.g., flag updates after construction) should call it.
