# 28.12 Knowledge-Specific Payload Encoding

§28 inherits the substrate's [payload encoding rules](../03_wire_protocol/04_payload_encoding.md) (rkyv 0.7 with `check_bytes`, big-endian multi-byte integers, CRC32C over the body). This file covers the **knowledge-specific** additions: opaque blob fields, large-blob handling, attribute / property blob encoding, evidence overflow.

## 1. Re-stated invariants (from substrate §03/04)

- Body bytes are rkyv-archived. The server calls `rkyv::check_archived_root::<T>` on every received body. Malformed archives → `MalformedRkyv` error, connection stays open ([substrate §03/10](../03_wire_protocol/10_errors.md)).
- Bodies are length-prefixed by the substrate frame header's `payload_len` (u24). 16 MiB - 1 hard cap.
- The header's `payload_crc32c` covers the body bytes.
- Multi-payload framing (`MPL` flag) extends a single logical payload across multiple frames.

§28 introduces no new framing — only conventions for what goes *inside* an rkyv body.

## 2. Opaque blob fields

Several §28 structs carry `Vec<u8>` fields that are *not* interpreted by the wire layer:

| Field | Carrier | Schema-aware decode by |
|---|---|---|
| `EntityCreateRequest.attributes_blob` | entity ops | phase 19 schema validator |
| `EntityView.attributes_blob` | entity reads | phase 19 SDK typed accessor |
| `RelationCreateRequest.properties_blob` | relation ops | phase 19 schema validator |
| `RelationView.properties_blob` | relation reads | phase 19 SDK |
| `StatementValueWire::Blob(_)` (inner) | statement values | application-level |
| `SchemaUploadRequest.schema_document` (String) | schema upload | parser |

### 2.1 Inner encoding (phase 19+)

`attributes_blob` and `properties_blob` are themselves **rkyv-encoded** maps:

```rust
// Logical shape (decoded after schema validation):
pub type AttributesMap = BTreeMap<String, StatementValueWire>;
```

The wire layer treats them as bytes. Phase 19's schema validator runs:

```rust
let map: AttributesMap = rkyv::check_archived_root::<AttributesMap>(blob)?;
let validated = schema.validate_attributes(entity_type, &map)?;
```

before the redb commit. Validation failures surface as `INVALID_ARGUMENT` (initial) or `ENTITY_TYPE_MISMATCH` (post-phase-19 with schema-aware errors).

### 2.2 Why nested rkyv?

Letting the inner shape be rkyv means SDK code can deserialize attributes once and pattern-match against typed accessors. The alternative (e.g. JSON or protobuf inside the rkyv outer struct) adds a second codec to the client and server. One codec, one validation pass.

### 2.3 Size cap

Per [`./04_validation.md`](./04_validation.md) §1: each opaque blob ≤ 64 KiB. Above that, callers split the payload into auxiliary records (statements with `EvidenceRef::Overflow`, etc.) or use a future "BLOB_PUT" pathway (post-v1.0).

## 3. Evidence encoding

Statements carry an `EvidenceRefWire` (see [`./06_statement_frames.md`](./06_statement_frames.md) §2.3):

```rust
pub enum EvidenceRefWire {
    Inline(Vec<u128>),     // ≤ 8 MemoryIds
    Overflow(WireUuid),    // EvidenceOverflowId
}
```

### 3.1 Inline path

`Inline` carries up to 8 packed `MemoryId`s as `u128` values (the same packing used in substrate ops). Cheap and zero-decode. Reject on the server if `len > 8`.

### 3.2 Overflow path

For larger evidence sets, the client pre-creates an `evidence_overflow` row out-of-band (via a future `EVIDENCE_PUT` opcode — post-v1.0; pre-v1.0 only the worker pipeline writes these rows) and references its UUIDv7 in `EvidenceRef::Overflow`. Reads dereference transparently.

### 3.3 No middle ground

There's deliberately no "13 inline" or "32 inline" tier. Either ≤ 8 (the common case for hand-authored statements) or overflow. Avoids tier-boundary edge cases in the storage layer.

## 4. Predicate strings

`predicate` fields are wire-carried as `String` in their canonical `"namespace:name"` form. The server interns them into a `predicates` redb table on first encounter (per [`../19_statements/00_purpose.md`](../19_statements/00_purpose.md) §"Predicate vocabulary"). Subsequent reads emit the same canonical form back to the wire.

### 4.1 Why strings, not interned `u32`?

Convenience for SDKs. A client constructing a `StatementCreateRequest` shouldn't have to lookup a `PredicateId` first. The intern step happens server-side on the create path.

### 4.2 Trade-off

~20-40 bytes per statement frame for the predicate string vs ~4 bytes for an interned id. Acceptable for current scale; revisit if `STATEMENT_LIST` streaming becomes bandwidth-bound at high QPS.

## 5. Time fields

All time fields are **unix nanoseconds**, `u64`. Matches substrate convention ([substrate §03/04](../03_wire_protocol/04_payload_encoding.md)).

### 5.1 Sentinel zero for "absent"

`valid_from_unix_nanos = 0`, `valid_to_unix_nanos = 0`, `event_at_unix_nanos = 0` mean "absent / not applicable" — not "January 1, 1970 00:00:00 UTC". Anyone encoding the unix epoch literally should encode as `1` ns instead (or accept the loss of one ns precision).

### 5.2 Why not `Option<u64>`?

Same reasoning as §11/§4 — `Option<u64>` archived directly via rkyv is awkward. Sentinel zero is simpler. Documented per-field where it matters.

## 6. Pagination cursors

`ENTITY_LIST`, `STATEMENT_LIST`, `RELATION_LIST_*`, `SCHEMA_LIST`, `EXTRACTOR_LIST` all carry an opaque `Vec<u8>` cursor field for continuation. The shape is **server-defined**:

```rust
// Currently (phase 16.6c): opaque rkyv blob containing the last seen key in the
// query's primary index. Concretely for ENTITY_LIST it's the last EntityId scanned,
// for STATEMENT_LIST it's a (subject, predicate, statement_id) triple, etc.
```

### 6.1 Why opaque?

Clients shouldn't depend on the cursor's internal shape — it's free to change between phases as indexes evolve. The wire shape is just "give me back what the server gave you, and you'll get the next page".

### 6.2 Cap

≤ 1 KiB per [`./04_validation.md`](./04_validation.md) §1. Malformed cursors (e.g. an `ENTITY_LIST` cursor fed to `STATEMENT_LIST`) error out with `INVALID_ARGUMENT`.

### 6.3 Stability across schema changes

A cursor issued under schema version N is **not** guaranteed valid under schema version N+1. Clients that span a `SCHEMA_UPDATED` event should restart their list scan from cursor `Vec::new()`.

## 7. CRC and validation order

§28 reuses substrate's [§03/11](../03_wire_protocol/11_validation.md) ordering:

1. Header magic + CRC verified by the substrate frame decoder.
2. Body bytes length-matched to `payload_len`.
3. Body CRC32C verified.
4. rkyv `check_archived_root::<T>` for the typed body shape.
5. Per-opcode handler-layer validation ([`./04_validation.md`](./04_validation.md)).

No knowledge-specific deviations.

## 8. Sentinel and reserved fields summary

For implementers, the complete list of "sentinel zero means absent" fields in §28:

| Field | Carrier | Sentinel |
|---|---|---|
| `EntityView.merged_into` | entity reads | `[0; 16]` |
| `EntityView.flags & TOMBSTONED` | entity reads | bit clear |
| `EntityResolveResponse.audit_id` | resolver | `[0; 16]` |
| `StatementView.subject_pending_audit_id` | statement reads | `[0; 16]` |
| `StatementView.superseded_by` | statement reads | `[0; 16]` |
| `StatementView.supersedes` | statement reads | `[0; 16]` |
| `StatementView.event_at_unix_nanos` (Fact/Pref) | statements | `0` |
| `StatementView.valid_from_unix_nanos`, `valid_to_unix_nanos` | statements | `0` |
| `RelationView.superseded_by` | relations | `[0; 16]` |
| `RelationView.valid_from_unix_nanos`, `valid_to_unix_nanos` | relations | `0` |

Reserved `u8` / `u32` byte / word fields that MUST be zero: none in §28 bodies (the substrate header carries the only reserved bytes; see [substrate §03/03](../03_wire_protocol/03_frame_header.md) §3.8).

## 9. Large-blob policy (informational)

§28 carries blobs ≤ 64 KiB inline. Blobs ≥ 64 KiB use:

- **For evidence:** `EvidenceRef::Overflow(EvidenceOverflowId)` referencing a separate `evidence_overflow` row.
- **For other large payloads** (e.g. an embedding blob): currently no §28 opcode carries one directly; future ops may use a "BLOB_PUT then reference by id" pattern.

The 64 KiB cap is per-blob, not per-frame. A frame may carry multiple ≤ 64 KiB blobs as long as the total fits in the 16 MiB - 1 frame budget. Multi-payload framing (substrate `MPL` flag) extends beyond a single frame's budget if needed.
