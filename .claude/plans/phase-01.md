# Plan: Phase 1 — Wire Protocol (retrospective + outlook)

**Status:** in-progress (8 of 11 sub-tasks complete as of 2026-05-10)
**Date drafted:** 2026-05-10 (retrospective for 1.1–1.8; forward-looking for 1.9–1.11)
**Author:** Claude (autonomous)

---

## 1. Scope

Phase 1 builds the wire protocol crate (`brain-protocol`). Outputs the byte-level codec needed before any networking or storage work:

- 32-byte fixed frame header with CRC32C-protected fields.
- Full opcode table per spec §03/05.
- Frame envelope with encode/decode and configurable payload bound.
- Round-trip + property + fuzz coverage.
- Request and response body codecs for all 25 client-bound and 25 server-bound opcodes.
- Handshake (HELLO / WELCOME / AUTH / AUTH_OK).
- Full error taxonomy mirroring spec §10.
- Companion `brain-core` types where the protocol reveals new fields.

**Out of scope (deferred to later phases):**

- Networking / TCP / TLS — Phase 9 (server) and the SDKs.
- Stream lifecycle management (allocation, demultiplexing) — Phase 9.
- Vector-blob composition into the trailing raw section — Phase 2 (storage) plus Phase 9 wiring.
- Auth backend integration (token verification, mTLS) — Phase 9.

## 2. Spec references

`spec/03_wire_protocol/` — entire directory is in scope.

| File | Purpose | Sub-task |
|---|---|---|
| `01_design_choices.md` | Why we chose this format | background reading |
| `03_frame_header.md` | 32-byte header layout | 1.1, 1.2, 1.4 |
| `04_payload_encoding.md` | rkyv + bytemuck split | 1.4, 1.7, 1.8 |
| `05_opcodes.md` | Opcode table | 1.3 |
| `06_handshake.md` | HELLO / WELCOME / AUTH / AUTH_OK | 1.9 |
| `07_request_frames.md` | 25 request payload schemas | 1.7 |
| `08_response_frames.md` | 25 response payload schemas | 1.8 |
| `09_streaming.md` | Stream lifecycle + EOS | 1.8 (codec shape only) |
| `10_errors.md` | Error categories + codes | 1.6, 1.8 |
| `11_validation.md` | Server-side validation rules | 1.5 (proptest), 1.10 (fuzz) |

## 3. External validation

- **rkyv 0.7** — pinned in workspace deps. Used for structured payload encoding. Choice documented in spec §03/04 §3 (zero-copy deserialization, validation via `check_archived_root`). Validated against rkyv 0.7 docs in Task 1.7's implementation; HRTB workaround for `DefaultValidator<'a>` documented in `rkyv_codec.rs`.
- **bytemuck 1.x with `derive` feature** — for raw vector blobs (`bytemuck::cast_slice<u8, f32>`) and for `Pod` derive on the packed `Header` struct. Choice documented in spec §03/04 §4.
- **crc32c crate** — Castagnoli polynomial (iSCSI variant). Verified correctness via the canonical `crc32c("123456789") == 0xE306_9283` test vector from RFC 3720.
- **proptest** — used for round-trip and decode-totality properties on `Frame` and `Opcode` (1024 cases each in dev).

## 4. Architecture sketch

```text
brain-protocol/src/
├── lib.rs                  module registry, re-exports, MAGIC/HEADER_SIZE/MAX_PAYLOAD_BYTES
├── header.rs               Header struct (repr(C, packed) + bytemuck::Pod), seal/validate
├── crc.rs                  header_crc / payload_crc CRC32C wrappers
├── opcode.rs               Opcode enum (49 variants + Error), from_u8, predicates
├── frame.rs                Frame { header, payload }, encode/decode/decode_with_max
├── error.rs                ProtocolError + ErrorCode + ErrorCategory
├── request.rs              RequestBody (25 variants) + wire-domain types
├── response.rs             ResponseBody (25 variants) + ERROR-frame mirror enums
├── handshake.rs            HelloPayload / WelcomePayload / AuthPayload / AuthOkPayload + negotiate (1.9)
└── rkyv_codec.rs           shared to_rkyv_bytes / from_rkyv_bytes (private)
```

Layering:

- `header` / `crc` are leaf modules; `frame` composes them.
- `error` defines `ProtocolError` (codec-level) plus `ErrorCode` / `ErrorCategory` (spec §10 mirror).
- `opcode` is consumed by `frame`, `request`, `response`.
- `request` / `response` share `rkyv_codec` and the wire-domain types (`WireUuid`, `WireMemoryId`, `MemoryKindWire`, etc.) defined in `request`.
- `handshake` (Task 1.9) plugs into `request` (HELLO, AUTH variants) and `response` (WELCOME, AUTH_OK).

## 5. Trade-offs considered

| Decision | Verdict |
|---|---|
| Multi-byte header fields stored as raw BE byte arrays (vs. native integers + endian conversion) | byte arrays — bytemuck-friendly, no padding holes, layout matches wire 1:1. |
| Wire-domain DTOs in `brain-protocol` (vs. rkyv derives on `brain-core` types) | DTOs — keeps `brain-core` free of rkyv dep; conversion at handler boundaries. |
| `ProtocolError` is a focused codec-level type; full §10 codes live in `ErrorCode` | yes — codec stays small, full taxonomy is one `match` away. |
| `ErrorCode` is `#[non_exhaustive]`; ERROR-frame body uses a closed `ErrorCodeWire` mirror | yes — preserves forward-compat in the canonical type while letting rkyv encode a closed enum. |
| `decode_with_max` accepts a configurable cap on top of the spec's hard 24-bit max | yes — defends against allocation-amplification before reading payload bytes. |
| `Header::new` panics on oversize `payload_len` (rather than returning `Result`) | yes — the 24-bit field physically can't carry more; programmer error, not runtime input. |

## 6. Risks / open questions

- **Naming drift between phase doc and spec.** Three concrete drifts found and corrected during 1.1–1.4: phase doc said little-endian (spec is BE), `to_le_bytes` for CRC (spec is BE), "ClientHello/ServerHello" (spec uses HELLO/WELCOME). Each correction landed via inline phase-doc footnotes; no spec changes.
- **`is_final` vs. EOS flag duplication** (spec §08 §3) — body and header both carry the end-of-stream signal. Codec preserves both; cross-check is owned by Phase 9's dispatcher.
- **Vector-blob composition** — `vector_offset` / `vector_dim` fields exist in payload structs but the actual `f32` byte placement is owned by the Frame layer (spec §03/04 §2). Tested at the structured-codec layer in 1.7; end-to-end test belongs to Phase 2/9.
- **`AuthMethod` numeric values** — spec doesn't pin them; we'll assign in 1.9 and lock them as wire-stable per spec §03/05 §7 (changes require a wire-version bump).

## 7. Test plan

| Sub-task | Tests added | Coverage |
|---|---|---|
| 1.1 Header | 12 unit tests | size/alignment, CRC seal+validate, magic/version/CRC/reserved rejection, Pod cast round-trip |
| 1.2 CRC wrappers | 5 unit tests | RFC 3720 vector, single-byte, splice-and-recompute, header_crc_excludes_self |
| 1.3 Opcode | 4 unit + 1 proptest | exhaustive `from_u8`, predicates, total-over-byte-range |
| 1.4 Frame | 11 unit tests | round-trip, all 7 phase-doc rejection cases, oversize cap, encode-reseals-against-drift |
| 1.5 Frame proptest | 2 proptests (1024 cases each) | encode-decode round-trip, decode-totality (no panics on arbitrary input) |
| 1.6 Errors | 4 unit tests | category mapping, retryability, ProtocolError→ErrorCode stable mapping, From for brain_core::Error |
| 1.7 Requests | 15 unit + 2 negative tests | per-variant round-trip across all 25 opcodes, response-opcode rejection, garbage-bytes rejection |
| 1.8 Responses | 15 unit + 3 negative tests | per-variant round-trip across all 25 opcodes, streaming sequence, is_final mapping, ErrorCode wire round-trip |
| 1.9 Handshake | planned: 9 tests | per-payload round-trip, all 3 auth methods, 5 negotiation scenarios |
| 1.10 Fuzz | planned: 60s fuzz run | no panics on arbitrary `Frame::decode` input |
| 1.11 Core types | as needed | type alignment with what the protocol exposed |

Total brain-protocol unit/proptest count after 1.8: **76 passing**. Workspace: **96 passing**.

## 8. Commit shape

13 commits to date on `feature/brain-protocol`:

```
8fe398c  Initial: spec + Phase 0 scaffold
1bbed33  0.fmt: apply rustfmt to scaffold
6732d5f  1.1: implement frame Header type with CRC validation
9dbae55  1.2: add CRC32C wrappers for header and payload
446ba0b  1.3: replace stub Opcode with full spec table
ef5e3a7  1.4: add Frame envelope with encode/decode
b81448c  1.5: add property tests for Frame round-trip and decode robustness
bf2bd05  chore: switch AUTONOMY §6 to feature-branch workflow
61e81b6  1.6: complete ProtocolError taxonomy with ErrorCode/Category
883f16a  1.7: implement RequestBody codec for all 25 server-bound opcodes
c5ca0ea  fix: stage rkyv-induced Cargo.lock changes from 1.7
89c5510  chore: add plan-first workflow and .claude/plans/
e7aeb98  1.8: implement ResponseBody codec for all 25 client-bound opcodes
```

Remaining commits (planned): 1.9, 1.10, 1.11, plus a phase-exit commit and the `phase-1-complete` tag.

## 9. Phase-exit checklist (looking ahead)

Per AUTONOMY §8, before tagging `phase-1-complete`:

- [ ] All sub-tasks 1.1–1.11 marked `[x]` in the phase doc.
- [ ] `just verify` green on `feature/brain-protocol`.
- [ ] Merge `feature/brain-protocol` → `dev`; verify green.
- [ ] Merge `dev` → `main`; tag `phase-1-complete` on `main`.
- [ ] Smoke-check that `brain-core` exports nothing the protocol depends on but doesn't have (Task 1.11 closes this).
- [ ] No `CONTEXT.md` outstanding.
