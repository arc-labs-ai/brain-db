# Phase 16 · Sub-task 16.6 — Entity wire opcodes (CREATE / GET / UPDATE / RENAME)

> **Scope revised on user direction:** Option C — widen the opcode to `u16` for *all* opcodes (substrate + knowledge). No protocol-version negotiation, no backward compatibility. Pre-v1.0 freedom; rewrite the substrate accordingly. Split into 16.6a / 16.6b / 16.6c.

## Decision recap

- Opcode becomes `u16`, big-endian on the wire.
- Substrate keeps its current low-byte values at high-byte `0x00`. `PING = 0x0010`, `ENCODE_REQ = 0x0020`, ..., `ADMIN_LIST_TOMBSTONED_RESP = 0x00E9`. Reads at-a-glance identically.
- Knowledge layer uses high-byte `0x01`, low-byte = §28's number. `SCHEMA_UPLOAD = 0x0120`, `ENTITY_CREATE = 0x0130`, `STATEMENT_CREATE = 0x0140`, `QUERY = 0x0160`, `ADMIN_REBUILD_INDEX = 0x0170`. §28's table reads verbatim with a `0x01` prefix.
- Knowledge **responses** use high-byte `0x01` and the substrate's "high bit on low byte" convention: `ENTITY_CREATE_RESP = 0x01B0`, `STATEMENT_CREATE_RESP = 0x01C0`, etc.
- Future namespaces (`0x02`–`0xFF`) reserved.
- No backward compat: pre-v1.0, no clients in the wild. Old serialized frames will not parse — that's intentional.

## Header redesign (32 bytes total, unchanged size)

```
| 0–3   | magic ("BRN0")          |
| 4     | version (u8)            |  ← stays at 1; this isn't a versioning bump
| 5–6   | opcode (u16, BE)        |  ← was: opcode u8 at byte 5
| 7     | flags (u8)              |  ← was: flags u16 at bytes 6-7; only 3 bits used (EOS/MPL/CMP)
| 8–11  | header_crc32c           |
| 12–15 | stream_id               |
| 16–18 | payload_len (u24, BE)   |
| 19    | reserved (u8, must be 0)|
| 20–23 | payload_crc32c          |
| 24–31 | reserved (8 bytes, 0)   |
```

Justifications:
- **Flags shrink to u8.** Spec §03/03 §2 already says bits 12-0 are reserved; only top 3 are used. u8 covers EOS / MPL / CMP and leaves 5 reserved bits — same headroom we had before. If we ever need more we steal from the byte-19 reserved.
- **Header CRC** still excludes its own 4 bytes; algorithm unchanged.
- **Size** stays exactly 32 — no transport-level surprises.

---

## 16.6a — Header + Opcode u16 refactor

**Reads:**
- `spec/03_wire_protocol/03_frame_header.md`
- `spec/03_wire_protocol/05_opcodes.md`
- `spec/03_wire_protocol/12_versioning.md`

**Writes (spec — user approves each edit):**
- `spec/03_wire_protocol/03_frame_header.md` — header table, validate algorithm, frame examples, endianness summary. Update bytes 5-7 layout.
- `spec/03_wire_protocol/05_opcodes.md` — opcode column becomes 4-hex-digit (e.g. `0x0010`). Note the namespace prefix scheme (0x00 = substrate, 0x01 = knowledge, 0x02-0xFF reserved). Server-bound is `0x0000–0x007F` *low byte*; client-bound is `0x0080–0x00FF` low byte; same convention per namespace.
- `spec/03_wire_protocol/12_versioning.md` — replace any "v1 wire compat" language with a "pre-v1.0; no backward compat guarantees" note. Specifically: this u16 change is permitted because v1.0 has not shipped.
- `spec/28_knowledge_wire_protocol/00_purpose.md` — prefix `0x01` namespace note at the top of the file; opcode column becomes 4-hex-digit (`0x0130`, etc.).

**Writes (code):**
- `crates/brain-protocol/src/header.rs`:
  - `Header.opcode: [u8; 2]`, `Header.flags: u8`, `Header.reserved_a: u8` (still at byte 19).
  - `Header::new(opcode: u16, flags: u8, stream_id: u32, payload_len: u32)`.
  - `Header.opcode_u16()`, `Header.flags_u8()` accessors.
  - `validate()` unchanged in shape; CRC algorithm unchanged.
- `crates/brain-protocol/src/opcode.rs`:
  - `#[repr(u16)] enum Opcode { ... }` — all variants get their u16 values. Substrate values widen to `0x00XX`.
  - `Opcode::from_u16(b: u16) -> Result<Self, ProtocolError>`; drop `from_u8`.
  - Module docstring rewritten for the namespace scheme.
- `crates/brain-protocol/src/frame.rs` — anywhere `header.opcode` is dereferenced; opcode comparisons.
- `crates/brain-protocol/src/error.rs` — `ProtocolError::UnknownOpcode { got: u16 }` (was u8).

**Tests in brain-protocol:**
- All existing `Header::new(0xNN, …)` calls update to `0x00NN`.
- Opcode round-trip tests gain a 4-hex-digit assertion.
- New: `Opcode::from_u16(0x0130).is_err()` — knowledge opcode rejected by substrate decoder (decode is split per namespace in 16.6c).
- Header pod-roundtrip preserves opcode + flag bytes.

**Done when:** `cargo test -p brain-protocol` clean; `cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-protocol --tests` clean.

**Pitfalls:**
- `Header` is `repr(C, packed)` + `bytemuck::Pod`. Re-derive doesn't tolerate a `u16` field directly — keep `opcode: [u8; 2]` so alignment-1 stays valid.
- Compile-time size assertion (`size_of::<Header>() == 32`) must still hold.
- `compute_header_crc` uses byte ranges 0–7 and 12–31 — unchanged.

**Commit:** `refactor(protocol): widen opcode to u16; split namespace (substrate 0x00, knowledge 0x01) — phase 16.6a`.

---

## 16.6b — Propagate u16 opcode through SDK + server

**Reads:**
- Output of 16.6a.

**Writes:**
- `crates/brain-server/src/network/{dispatch,connection,subscribe}.rs` — every `opcode` read/match.
- `crates/brain-http/src/**` — same.
- `crates/brain-sdk-rust/src/proto/handshake.rs`, `src/ops/{encode,recall,plan,reason,forget,link,unlink,subscribe,txn,stream,common}.rs`, `src/client/mod.rs`, `src/pool/connection.rs` — frame builder calls.
- `crates/brain-ops/tests/correctness.rs`.
- All affected tests under `crates/brain-{server,sdk-rust}/tests/*.rs`.

**Approach:**
- Search `Opcode::` (~256 hits) and `header.opcode` (a dozen). Update each to the new signature.
- Where a test asserts on the raw byte (e.g. `bytes[5] == 0x10`), widen to a 2-byte check at offset 5-6.

**Done when:**
- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- All existing server/SDK tests pass.
- Clippy clean (`-D warnings`).

**Pitfalls:**
- Some SDK tests hard-code frame bytes; those need byte-level updates.
- `dispatch.rs` may have an opcode-keyed `HashMap`/`match` that needs widening.

**Commit:** `refactor(server,sdk): propagate u16 opcode through call-sites — phase 16.6b`.

---

## 16.6c — Knowledge namespace + entity wire ops

**Reads:**
- `spec/28_knowledge_wire_protocol/00_purpose.md` (now with the 0x01 prefix note from 16.6a).
- `spec/18_entities/00_purpose.md`, `spec/18_entities/02_storage.md`.

**Writes (brain-protocol):**
```
crates/brain-protocol/src/knowledge/
├── mod.rs            # KnowledgeOpcode enum (u16, 0x0120-0x017F range), KnowledgeNamespace tag
├── entity_req.rs     # EntityCreateRequest, EntityGetRequest, EntityUpdateRequest, EntityRenameRequest
└── entity_resp.rs    # EntityCreateResponse, EntityGetResponse, EntityUpdateResponse, EntityRenameResponse
```
- New variants on `Opcode`: `EntityCreateReq = 0x0130`, `EntityCreateResp = 0x01B0`, `EntityGetReq = 0x0131`, `EntityGetResp = 0x01B1`, `EntityUpdateReq = 0x0132`, `EntityUpdateResp = 0x01B2`, `EntityRenameReq = 0x0133`, `EntityRenameResp = 0x01B3`. (Other §28 opcodes are added in later phases — only these four this sub-task.)
- Add `RequestBody::EntityCreate(EntityCreateRequest)`, `…GET`, `…UPDATE`, `…RENAME` variants (or one `Knowledge(KnowledgeRequest)` umbrella; decision noted below).
- Same for `ResponseBody`.

**Decision — flat vs umbrella variants:** Flat. The existing `RequestBody` is already flat (one variant per opcode). Knowledge ops join it directly. Keeps dispatch simple, mirrors substrate style.

**Writes (brain-server):**
```
crates/brain-server/src/handlers/knowledge/
├── mod.rs            # re-exports
└── entity.rs         # handle_entity_create, _get, _update, _rename
```
- `network::dispatch` gains four match arms.
- Each handler:
  1. Validates the request (canonical_name non-empty after `normalize_name`; entity_type_id exists in the registry).
  2. Calls `brain-metadata::entity_ops::{entity_put, entity_get, entity_update, entity_rename}` against the shard's `MetadataDb`.
  3. Maps `EntityOpError` → wire error code per spec §28 (0x30 NotFound, 0x31 TypeMismatch, ...). Error frames carry the u16 error code in the body — *existing* error frame mechanism, no changes.

**Writes (tests):**
- `crates/brain-protocol/src/knowledge/entity_req.rs` — rkyv roundtrip per struct.
- `crates/brain-protocol/src/knowledge/entity_resp.rs` — same.
- `crates/brain-server/tests/knowledge_entity_wire.rs` — end-to-end:
  - `ENTITY_CREATE` (Person, "Alice") → non-zero EntityId.
  - `ENTITY_GET` → matches.
  - `ENTITY_UPDATE` (add attribute) → ack.
  - `ENTITY_RENAME` ("Alice Cooper", `move_to_alias=true`) → ack; subsequent `ENTITY_GET` shows new canonical + "Alice" in `aliases`.
  - Negative: `ENTITY_GET` with random UUID → `0x0130 ENTITY_NOT_FOUND`.
  - Negative: `ENTITY_CREATE` with unknown type id → `0x0131 ENTITY_TYPE_MISMATCH`.

**Done when:**
- All tests green on zigbuild.
- An entity created via `ENTITY_CREATE` is readable via `ENTITY_GET` and persists across redb reopen.

**Pitfalls:**
- `EntityAttributes(pub Vec<u8>)` — confirm wire encoding: rkyv `Vec<u8>` is fine.
- Rename atomicity: 16.2's `entity_rename` already does the canonical-swap + alias-append in one redb txn. Reconfirm.
- `EntityId` (UUIDv7, 16 bytes) — wire type is `WireUuid = [u8; 16]`. Use the same `From` impls as substrate `MemoryId`.

**Commit:** `feat(protocol,server): ENTITY_CREATE/GET/UPDATE/RENAME wire ops (phase 16.6c)`.

---

## Out of scope (this sub-task tree)

- `ENTITY_MERGE / UNMERGE / RESOLVE / LIST / TOMBSTONE` (0x0134-0x0138) — 16.7-16.9.
- Statement / relation / query / admin / schema / extractor opcodes — phases 17–24.
- SDK helpers for entity CRUD — 16.8.
- `ENTITY_RESOLVE` exposing 16.5's resolver — 16.7 (chosen).

## Risks

- **Big rebase.** ~256 opcode call-sites. If any are missed, tests will fail loudly — that's fine.
- **Hard-coded byte offsets in tests.** Anything reading `frame[5]` for opcode must move to `frame[5..7]`.
- **Spec edits.** Per CLAUDE.md §2 (spec is read-only, changes go through the user), I'll surface each spec edit as a diff in a chat message before writing. User can approve in bulk or per-file.

## Order of operations

1. 16.6a — header + Opcode refactor + spec edits → small green build (brain-protocol only).
2. 16.6b — fan out through the rest of the workspace → full workspace green.
3. 16.6c — add the knowledge namespace + entity ops → end-to-end green.

Each is its own commit on `feature/phase-16-entity-layer`.
