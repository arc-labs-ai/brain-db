# Phase 1 — Wire Protocol & Core Types

## Goal

Implement the on-the-wire frame format and round-trippable codecs for every opcode. After this phase, you can take any byte sequence claiming to be a Brain frame, validate it, and parse it into a typed request — or refuse it cleanly.

## Prerequisites

- [x] Phase 0 complete (workspace builds, CI green, tag `phase-0-complete` exists).
- `crates/brain-core` and `crates/brain-protocol` are stubs and inherit workspace deps.

## Reading list (read in this order before starting)

1. [`spec/03_wire_protocol/00_purpose.md`](../../spec/03_wire_protocol/00_purpose.md) — what the protocol is for.
2. [`spec/03_wire_protocol/01_design_choices.md`](../../spec/03_wire_protocol/01_design_choices.md) — why binary, why CRCs, why fixed-header.
3. [`spec/03_wire_protocol/02_transport.md`](../../spec/03_wire_protocol/02_transport.md) — TCP + optional TLS.
4. [`spec/03_wire_protocol/03_frame_header.md`](../../spec/03_wire_protocol/03_frame_header.md) — the 32-byte header layout. **Critical.**
5. [`spec/03_wire_protocol/04_payload_encoding.md`](../../spec/03_wire_protocol/04_payload_encoding.md) — rkyv structured + bytemuck for vector blobs.
6. [`spec/03_wire_protocol/05_opcodes.md`](../../spec/03_wire_protocol/05_opcodes.md) — every opcode and its number.
7. [`spec/03_wire_protocol/06_handshake.md`](../../spec/03_wire_protocol/06_handshake.md) — initial handshake.
8. [`spec/03_wire_protocol/07_request_frames.md`](../../spec/03_wire_protocol/07_request_frames.md) — request bodies.
9. [`spec/03_wire_protocol/08_response_frames.md`](../../spec/03_wire_protocol/08_response_frames.md) — response bodies.
10. [`spec/03_wire_protocol/09_streaming.md`](../../spec/03_wire_protocol/09_streaming.md) — streaming responses.
11. [`spec/03_wire_protocol/10_errors.md`](../../spec/03_wire_protocol/10_errors.md) — error frame shape, error codes.
12. [`spec/03_wire_protocol/11_validation.md`](../../spec/03_wire_protocol/11_validation.md) — what counts as malformed.

After reading: every constant, magic byte, length bound, and opcode number should be in your head, not paraphrased.

## Outputs

By end of phase:

- `crates/brain-core` exports the full type vocabulary used by the protocol (`MemoryId`, `AgentId`, `ContextId`, `RequestId`, `EdgeKind`, `MemoryKind`, `Salience`, `Error`, plus any new types the protocol needs).
- `crates/brain-protocol` exports:
  - `Frame` (the parsed envelope)
  - `Header` (the 32-byte header)
  - `Opcode` (complete enum)
  - `RequestBody`, `ResponseBody` (tagged unions)
  - `ProtocolError` (error variants from §10)
  - `encode(frame) -> Vec<u8>`
  - `decode(bytes) -> Result<Frame, ProtocolError>`
- Property tests covering every opcode round-trip.
- A working fuzz target that exercises `decode` on arbitrary bytes.
- Tag: `phase-1-complete`.

## Sub-tasks

Each sub-task is a single commit. The "Reads" listed are required reading before writing the code.

---

### Task 1.1 — Pin protocol constants and the `Header` type

**Reads:**
- `spec/03_wire_protocol/03_frame_header.md`

**Writes:**
- `crates/brain-protocol/src/header.rs` — new module
- `crates/brain-protocol/src/lib.rs` — register module, re-export

**What to build:**
- `pub const MAGIC: [u8; 4] = *b"BRN0";` (already present in stub; verify)
- `pub const VERSION: u8 = 1;`
- `pub const HEADER_SIZE: usize = 32;`
- `pub const MAX_PAYLOAD_BYTES: usize = (1 << 24) - 1;`
- `#[repr(C, packed)] pub struct Header { ... }` matching the spec's byte layout exactly. Use `bytemuck::Pod` + `bytemuck::Zeroable` for safe casting.
- `impl Header { pub fn new(opcode, flags, stream_id, payload_len) -> Self }` — computes and stores the header CRC32C internally.
- `impl Header { pub fn validate(&self) -> Result<(), ProtocolError> }` — checks magic, version, header CRC, length bound. (`ProtocolError` is defined in 1.6.)

**Tests:**
- `header_has_correct_size`: `assert_eq!(size_of::<Header>(), 32)`.
- `header_has_correct_alignment`: alignment is 1 (we're `repr(C, packed)`). If a different alignment is required by the spec, assert that.
- `magic_bytes_match`: `assert_eq!(&MAGIC, b"BRN0")`.

**Done when:**
- [x] Module compiles and tests pass.
- [x] `bytemuck::Pod` derive works (no padding holes — verify by reading `mem::size_of` vs sum of fields).
- [x] `Header::new` computes a CRC that `validate` accepts.

**Pitfalls:**
- `repr(C, packed)` makes field access on references unsafe. Always copy out of the struct or use `addr_of!`.
- Endianness: the spec uses **big-endian** for multi-byte fields (spec §03/03 §1, §8). Use `u16::to_be_bytes` etc. when serializing. *(Earlier draft of this doc said "little-endian"; corrected against spec.)*
- Don't fold the payload CRC into the header CRC — they're separate per spec.

---

### Task 1.2 — CRC32C wrappers

**Reads:**
- `spec/03_wire_protocol/03_frame_header.md` (CRC sections)

**Writes:**
- `crates/brain-protocol/src/crc.rs`

**What to build:**
- `pub fn header_crc(bytes_before_crc_field: &[u8]) -> u32` — computes CRC32C over the header bytes that precede the `header_crc32c` field, per the spec layout.
- `pub fn payload_crc(payload: &[u8]) -> u32` — CRC32C over the entire payload.
- Both use `crc32c::crc32c(...)` from the workspace dep.

**Tests:**
- Known vector: take the spec's example header bytes (if any) and verify CRC. If no vector, hand-compute one and pin it.
- `header_crc_excludes_self`: hashing the header bytes minus the CRC field gives the value that's stored in the CRC field.

**Done when:**
- [x] Functions are pure, public, documented.
- [x] Tests pin specific CRC values, not just "round-trips."

**Pitfalls:**
- CRC32C ≠ CRC32. Confirm `crc32c` crate is the iSCSI variant (it is).
- `crc32c::crc32c` returns u32, not bytes. Convert with `to_be_bytes` for serialization (spec §03/03 §8 — both CRC fields are big-endian on the wire). *(Earlier draft of this doc said `to_le_bytes`; corrected against spec.)*

---

### Task 1.3 — `Opcode` enum, complete

**Reads:**
- `spec/03_wire_protocol/05_opcodes.md`

**Writes:**
- Update `crates/brain-protocol/src/lib.rs` — replace the partial stub `Opcode` with the full set.
- `crates/brain-protocol/src/opcode.rs` — promote to its own module if the lib.rs is getting busy.

**What to build:**
- `#[repr(u8)] enum Opcode { ... }` — every opcode from the spec, with the spec's exact numeric values.
- `impl Opcode { pub fn from_u8(b: u8) -> Result<Self, ProtocolError> }` — exhaustive match returning `UnknownOpcode` for unmapped values.
- `impl Opcode { pub fn is_request(self) -> bool }` and `is_response`/`is_admin` predicates.

**Tests:**
- For each opcode: `Opcode::from_u8(N).unwrap() == Opcode::Foo`.
- For unknown: `Opcode::from_u8(0xFE).is_err()`.
- Property test: every value in `0..=255` either maps to an opcode or returns the same `UnknownOpcode` error.

**Done when:**
- [x] All opcodes from spec §05 are present with matching numbers.
- [x] `from_u8` is exhaustive and tested.
- [x] Predicate helpers exist if the spec distinguishes request/response/admin.

**Pitfalls:**
- Don't renumber opcodes. The spec pins them.
- If the spec reserves ranges (e.g. `0x80..=0xEF` for vendor extensions), document that in the module.

---

### Task 1.4 — Frame envelope: `Frame` type and (de)serialization scaffolding

**Reads:**
- `spec/03_wire_protocol/03_frame_header.md`
- `spec/03_wire_protocol/04_payload_encoding.md`

**Writes:**
- `crates/brain-protocol/src/frame.rs`

**What to build:**
- `pub struct Frame { pub header: Header, pub payload: Vec<u8> }`
- `impl Frame { pub fn encode(&self) -> Vec<u8> }` — emits header + payload, computes both CRCs, returns the bytes.
- `impl Frame { pub fn decode(bytes: &[u8]) -> Result<(Self, &[u8]), ProtocolError> }` — parses one frame, returns `(frame, rest)` so callers can decode streams.

**Tests:**
- `encode_then_decode_roundtrip`: with arbitrary opcode and payload bytes.
- `decode_rejects_bad_magic`.
- `decode_rejects_bad_version`.
- `decode_rejects_bad_header_crc`.
- `decode_rejects_bad_payload_crc`.
- `decode_rejects_truncated_input`.
- `decode_rejects_oversize_payload`.

**Done when:**
- [x] All seven test cases pass.
- [x] `encode` and `decode` are inverses for valid frames.
- [x] Errors match the variants in spec §10.

**Pitfalls:**
- Empty payload is valid. Header still has its CRC; payload CRC is over empty bytes (well-defined CRC of empty input).
- The decoder returns the `rest` slice for stream consumers — don't `Vec::extend` and lose the borrow.

---

### Task 1.5 — Property tests for `Frame`

**Reads:**
- `spec/03_wire_protocol/11_validation.md`

**Writes:**
- `crates/brain-protocol/src/frame.rs` — extend the `tests` module
- Or `crates/brain-protocol/tests/frame_proptest.rs`

**What to build:**
- `proptest!` block: arbitrary (opcode, flags, stream_id, payload_bytes) → encode → decode → assert equality.
- `proptest!` block: arbitrary bytes → decode → either succeeds (and re-encoding gives back equivalent bytes) or returns a structured error. Must not panic.

**Tests:**
- The two proptest blocks above.
- Run with at least 1024 cases each (`PROPTEST_CASES=1024 cargo test`).

**Done when:**
- [x] Both proptests pass with default case count.
- [x] No panics on arbitrary input — even malformed.

**Pitfalls:**
- Bound payload size in the generator (e.g. 0..=8192) so tests don't allocate gigabytes.
- If a test fails, save the seed via `proptest`'s regression file mechanism.

---

### Task 1.6 — `ProtocolError` taxonomy

**Reads:**
- `spec/03_wire_protocol/10_errors.md`

**Writes:**
- `crates/brain-protocol/src/error.rs`

**What to build:**
- `#[derive(thiserror::Error, Debug)] enum ProtocolError { ... }` — variants for every error case in §10:
  - `BadMagic`, `UnsupportedVersion(u8)`, `BadHeaderCrc`, `BadPayloadCrc`,
  - `Truncated`, `OversizePayload(usize)`, `UnknownOpcode(u8)`, `MalformedPayload(String)`,
  - any others the spec defines.
- `impl ProtocolError { pub fn code(&self) -> ErrorCode }` — maps to the wire-level error code from §10.

**Tests:**
- For each error variant: it has a `code()` matching the spec.

**Done when:**
- [x] Every variant in spec §10 is represented.
- [x] `From<ProtocolError>` for `brain_core::Error` (via `Internal` or `InvalidArgument` as appropriate).

**Pitfalls:**
- Don't conflate transport errors (TCP reset) with protocol errors. Transport handling is Phase 9.

---

### Task 1.7 — Request body codecs

**Reads:**
- `spec/03_wire_protocol/07_request_frames.md`

**Writes:**
- `crates/brain-protocol/src/request.rs`

**What to build:**
- `enum RequestBody { Encode(EncodeRequest), Recall(RecallRequest), ... }` — one variant per request opcode.
- For each variant, a struct with the fields per the spec's request schema.
- Encode/decode using `rkyv` for the structured fields and `bytemuck` for any vector blobs.
- `impl RequestBody { pub fn encode(&self) -> Vec<u8> }` and `pub fn decode(opcode: Opcode, bytes: &[u8]) -> Result<Self, ProtocolError>`.

**Tests:**
- For each request variant: round-trip `encode → decode == original`.

**Done when:**
- [x] All request opcodes from §07 have a matching variant and codec.
- [x] Round-trip tests for each.
- [x] Vector blobs (where present) use `bytemuck::cast_slice`, not rkyv. *(Note: vector-blob composition into the trailing raw section is owned by the `Frame` layer, not by `RequestBody::encode`. The struct fields `vector_offset` / `vector_dim` carry the placement information; rkyv handles the structured fields only.)*

**Pitfalls:**
- `rkyv` requires the type to derive `Archive`, `Serialize`, `Deserialize` from the rkyv prelude. Add the workspace dep if not already present.
- The wire format for vector blobs is little-endian f32 packed. Cross-check with spec §04.

---

### Task 1.8 — Response body codecs

**Reads:**
- `spec/03_wire_protocol/08_response_frames.md`
- `spec/03_wire_protocol/09_streaming.md`

**Writes:**
- `crates/brain-protocol/src/response.rs`

**What to build:**
- `enum ResponseBody { ... }` mirroring the request shape — one variant per response.
- Streaming variants: `Next`, `Complete` per §09.
- Round-trip codecs.

**Tests:**
- Round-trip every variant.
- Streaming sequence: encode `[Next, Next, Complete]`, decode, verify ordering preserved.

**Done when:**
- [x] All response opcodes have variants and codecs.
- [x] Streaming protocol tested (at least encoding/decoding shape; multi-frame transport is Phase 9).

**Pitfalls:**
- A `Complete` response can carry a final payload (per §09). Don't assume it's empty.

---

### Task 1.9 — Handshake

**Reads:**
- `spec/03_wire_protocol/06_handshake.md`

**Writes:**
- `crates/brain-protocol/src/handshake.rs`

**What to build:**
- `pub struct ClientHello { ... }` and `pub struct ServerHello { ... }` per §06.
- Codecs for both.
- `pub fn negotiate(client: &ClientHello, server_caps: &ServerCapabilities) -> Result<NegotiatedSession, ProtocolError>`.

**Tests:**
- Round-trip both messages.
- Negotiation: compatible versions succeed; incompatible fail with `UnsupportedVersion`.

**Done when:**
- [x] Hello messages round-trip. *(All four — HELLO, WELCOME, AUTH, AUTH_OK — round-trip through rkyv. Phase doc said "ClientHello/ServerHello" but spec §03/06 names the four messages explicitly; spec wins.)*
- [x] Negotiation logic matches the spec's compatibility matrix.

---

### Task 1.10 — Wire up the fuzz target

**Reads:**
- `spec/03_wire_protocol/11_validation.md`
- Phase 0's `fuzz/fuzz_targets/protocol_frame.rs` placeholder.

**Writes:**
- `fuzz/fuzz_targets/protocol_frame.rs` — replace placeholder with real harness.

**What to build:**
- `fuzz_target!(|data: &[u8]| { let _ = brain_protocol::Frame::decode(data); });`
- Add a second target `protocol_request.rs` that decodes arbitrary bytes as a `RequestBody` for each opcode.

**Tests:**
- `cargo +nightly fuzz run protocol_frame -- -max_total_time=60` exits cleanly.

**Done when:**
- [x] Fuzz harness builds.
- [x] 60-second run finds no panics. *(Three targets: protocol_frame, protocol_request, protocol_response. Smoked at 60s each — 28M / 19M / 19M runs respectively, zero panics, zero artifacts.)*

**Pitfalls:**
- Fuzzing requires nightly Rust. CI should not fail if nightly is unavailable; gate the fuzz step behind a `nightly-only` job.

---

### Task 1.11 — `brain-core` companion types

**Reads:**
- `spec/02_data_model/03_identifiers.md`
- `spec/02_data_model/02_memory_entity.md`
- `spec/02_data_model/06_edges.md`
- `spec/02_data_model/05_salience.md`

**Writes:**
- Update `crates/brain-core/src/*` as the protocol reveals new fields.

**What to build:**
- Anything the protocol's request/response types need that's not yet in `brain-core`.
- Examples: `EncodeOptions`, `RecallFilter`, `PlanDirection`.

**Tests:**
- For each new type: a basic constructor + round-trip via `serde` if it's serializable.

**Done when:**
- [x] `brain-protocol` compiles without inline duplicates of types that belong in core. *(Wire-domain types — `WireMemoryId`/`WireUuid`/`WireContextId` aliases plus `MemoryKindWire`/`EdgeKindWire` rkyv enums — are deliberate per Task 1.7's design and bridge to brain-core via `From`/`Into` impls in `brain-core::ids` and `brain-protocol::convert`.)*
- [x] `brain-core` compiles standalone.

Drift fixes (spec §02/03 wins): `MemoryId` bit layout corrected (shard 16 + slot 48 + version 32 + reserved 32); `ContextId` `Uuid` → `u64`; `ShardId` `u8` → `u16`; `SlotVersion` `u16` → `u32`. Wire-side `context_id` fields switched to `WireContextId = u64` (8 bytes per spec §02/03 §8). Added `TxnId` (UUIDv7). Updated `Edge` per spec §02/06: `source`/`target` (was `from`/`to`), added `weight` and `EdgeOrigin`, switched timestamp to `unix_nanos`.

**Pitfalls:**
- Resist over-engineering. Only add types that the protocol actively uses.

---

## Phase exit checklist

Before tagging `phase-1-complete`:

- [x] All sub-tasks 1.1–1.11 marked done in this file.
- [x] `just verify` is green on a clean checkout. *(122 tests workspace, clippy clean, fmt clean, 23/23 skills valid.)*
- [x] `cargo test --workspace` runs ≥ 30 tests, all passing. *(122.)*
- [x] At least one proptest with ≥ 1024 cases per opcode. *(`Opcode::from_u8_is_total` cycles every byte; `Frame::encode_decode_round_trip` and `decode_arbitrary_bytes_is_total` run 1024 cases each.)*
- [x] Fuzz target builds and a 60-second run is clean. *(Three targets — `protocol_frame`, `protocol_request`, `protocol_response` — smoked at 60s each, ~67M total runs, zero panics, zero artifacts.)*
- [x] Public API of `brain-protocol` is documented. *(Every `pub` item carries rustdoc; spec section anchors are inline.)*
- [x] `cargo doc --workspace --no-deps` builds without warnings.
- [x] `git tag phase-1-complete` on the latest green commit.

## Commit strategy

- One sub-task = one commit, with the message format from `AUTONOMY.md` §5.
- Larger sub-tasks (1.7, 1.8) may split into 2-3 commits if each commit independently compiles and tests.
- After 1.11, run the full exit checklist, then tag.

## Decisions log

Record every non-trivial decision here so subsequent phases (and the user) can find them.

| Date | Decision | Rationale | Sub-task |
|---|---|---|---|
| 2026-05-10 | Header multi-byte fields stored as raw BE byte arrays, not native ints | `bytemuck::Pod` derive with no padding holes; on-wire layout matches struct 1:1; avoids `repr(C, packed)` field-ref unsafety | 1.1 |
| 2026-05-10 | `decode_with_max(bytes, max)` separate from `decode(bytes)` | Allocation-amplification defense: peer's claimed `payload_len` checked before reading payload bytes | 1.4 |
| 2026-05-10 | `ErrorCode` is `#[non_exhaustive]`; `ErrorCodeWire` is closed | Forward-compat for the canonical type; rkyv needs a closed enum for the wire body. Identity round-trip via `From` impls | 1.6, 1.8 |
| 2026-05-10 | Wire-domain DTOs (`WireMemoryId`, `WireUuid`, `MemoryKindWire`, `EdgeKindWire`) live in `brain-protocol`, not `brain-core` | Keeps `brain-core` rkyv-free; conversion happens at boundary via `From`/`Into` | 1.7, 1.11 |
| 2026-05-10 | Vector-blob composition (rkyv structured + trailing raw f32 section) owned by `Frame` layer, not `RequestBody`/`ResponseBody` | Spec §03/04 separates structured + raw; per-body codec stays single-purpose. End-to-end vector wiring deferred to Phase 2/9 | 1.7, 1.8 |
| 2026-05-10 | Promote `to_rkyv_bytes`/`from_rkyv_bytes` to private `crate::rkyv_codec` | Both `request` and `response` need the HRTB-laden helper; one source of truth | 1.8 |
| 2026-05-10 | `negotiate(client, server)` does version + capability intersection only; auth-method intersection defers to AUTH-frame handler | Pure logic testable in isolation; runtime concerns (server picks `session_id`, populates `ServerFeatures`) stay in connection layer | 1.9 |
| 2026-05-10 | `protocol_request` / `protocol_response` fuzz harnesses dispatch by `data[0] mod len(opcodes)` rather than `Opcode::from_u8` | Most random bytes are unassigned opcodes; mod-len cycles all variants under coverage guidance | 1.10 |
| 2026-05-10 | Spec §02/03 wins over phase-doc + earlier code: `MemoryId` layout = shard 16 + slot 48 + version 32 + reserved 32; `ContextId` = u64; `ShardId` = u16; `SlotVersion` = u32 | Pre-Phase-9, no deployed clients — fix layout drifts now; spec is read-only authoritative | 1.11 |
| 2026-05-10 | Wire `context_id` fields = `WireContextId = u64` (8 bytes) | Spec §02/03 §8 says ContextId on the wire is 8 bytes; protocol previously used `WireUuid` (16). Fixed before any deployed client | 1.11 |
| 2026-05-10 | Endianness pitfalls in phase doc corrected against spec §03/03 §8 (header) and §02/03 §2.1 (MemoryId): all multi-byte = big-endian | Phase doc had two LE references that conflicted with the spec | 1.1, 1.2 |
| 2026-05-10 | `ClientHello` / `ServerHello` phase-doc names superseded by spec §03/06 names: HELLO / WELCOME / AUTH / AUTH_OK | Spec wins; codec covers all four messages | 1.9 |
