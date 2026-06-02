# 04.02 Wire Format

The wire format covers two layers: the 32-byte frame header that prefixes every frame, and the payload encoding (CBOR for structured data, little-endian f32 for raw vector bytes).

> The opcode is a big-endian `u16` (bytes 5–6) and `flags` is a single byte (byte 7). The namespace byte (high byte of opcode) is `0x00` for substrate ops, `0x01` for typed-graph ops — see [`03_opcodes.md`](03_opcodes.md).

## Frame Header

### 1. The 32-byte header

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|              magic = "BRN0" (4 bytes)                         |
+---------------+-------------------------------+---------------+
|   version (8) |          opcode (16)          |   flags (8)   |
+---------------+-------------------------------+---------------+
|                  header_crc32c (32)                           |
+---------------------------------------------------------------+
|                     stream_id (32)                            |
+---------------------------------------------------------------+
|   payload_len (24, big-endian)                |   reserved(8) |
+-----------------------------------------------+---------------+
|                  payload_crc32c (32)                          |
+---------------------------------------------------------------+
|                       reserved (32)                           |
+---------------------------------------------------------------+
|                       reserved (32)                           |
+---------------------------------------------------------------+
```

Total: 32 bytes.

Field-by-field:

| Bytes | Field | Type | Purpose |
|---|---|---|---|
| 0–3 | `magic` | 4 ASCII chars | Identifies a Brain frame: `"BRN0"` (0x42 0x52 0x4E 0x30) |
| 4 | `version` | `u8` | Protocol version. Initially 1; bumps on incompatible changes. |
| 5–6 | `opcode` | `u16` (big-endian) | The operation type. High byte = namespace (0x00 substrate, 0x01 typed graph); low byte = op index. See [`03_opcodes.md`](03_opcodes.md). |
| 7 | `flags` | `u8` | Frame-level flags (see §2). |
| 8–11 | `header_crc32c` | `u32` | CRC32C of bytes 0–7 plus bytes 12–31 (i.e., the rest of the header excluding this field, treated as zero during computation). |
| 12–15 | `stream_id` | `u32` | The stream this frame belongs to (see [`06_streaming.md`](06_streaming.md)). |
| 16–18 | `payload_len` | `u24` (24-bit big-endian) | Payload length in bytes; max 16,777,215 (16 MiB - 1). |
| 19 | reserved | `u8` | Must be zero; reserved for future expansion. |
| 20–23 | `payload_crc32c` | `u32` | CRC32C of the payload (zero if `payload_len = 0`). |
| 24–31 | reserved | 8 bytes | Reserved for future expansion. Must be zero. |

All multi-byte integers are **big-endian**.

### 2. Flags

The 8-bit `flags` field encodes per-frame metadata:

```
bit 7   6   5   4   3   2   1   0
   +---+---+---+---+---+---+---+---+
   |EOS|MPL|CMP|       reserved    |
   +---+---+---+---+---+---+---+---+
```

| Bit | Name | Meaning |
|---|---|---|
| 7 | `EOS` | End of stream — last frame of this stream. |
| 6 | `MPL` | Multi-payload — payload spans multiple frames; concatenate to reconstruct. |
| 5 | `CMP` | Compressed — payload is zstd-compressed. (Reserved; not currently used.) |
| 4-0 | reserved | Must be zero. |

The flags `EOS` and `MPL` are mutually compatible: a multi-payload final frame has both. A single-frame final response has only `EOS`.

### 3. Field details

#### 3.1 magic

The 4-byte sequence `0x42 0x52 0x4E 0x30` (ASCII `"BRN0"`).

The `0` in the trailing position is a generation marker. If a fundamentally incompatible new framing is ever needed, a marker like `"BRN1"` would be used. Within `"BRN0"`-framed protocols, the `version` field handles compatible evolution.

A reader that sees a different magic on the first frame of a connection MUST close the connection — this isn't a Brain frame.

#### 3.2 version

The protocol version. Currently **1**.

The version is checked at handshake time (the `HELLO` frame's negotiation). Once negotiated, all subsequent frames on the connection MUST have the same version. A frame with a different version is a protocol error and the connection is closed.

#### 3.3 opcode

The operation type. See [`03_opcodes.md`](03_opcodes.md) for the full opcode table (substrate primitives at `0x00xx` and typed-graph operations at `0x01xx`).

The opcode is a big-endian `u16` split into two bytes:

- **High byte — namespace.**
  - `0x00` — substrate (cognitive primitives, connection management, admin).
  - `0x01` — typed graph (schema, entities, statements, relations, queries, extractors).
  - `0x02`–`0xFF` — reserved for future namespaces.
- **Low byte — operation index within the namespace.** Low byte `< 0x80` is server-bound (C → S, request); low byte `≥ 0x80` is client-bound (S → C, response). The direction rule applies independently within each namespace.

Examples: `0x0020` is substrate `ENCODE_REQ`; `0x00A0` is substrate `ENCODE_RESP`; `0x0130` is knowledge `ENTITY_CREATE` (request); `0x01B0` is knowledge `ENTITY_CREATE_RESP`.

#### 3.4 payload_len

The length of the payload in bytes, as a 24-bit big-endian unsigned integer. Maximum: 16,777,215 (just under 16 MiB).

A frame with `payload_len = 0` has no payload bytes after the header. Both `EOS`-only frames and pure ACK frames typically have empty payloads.

For payloads larger than 16 MiB, use multi-payload framing: split the payload across multiple frames, all but the last having `MPL` set, and concatenate at the receiver.

#### 3.5 stream_id

The stream this frame belongs to. See [`06_streaming.md`](06_streaming.md) for the streaming model.

`stream_id = 0` is reserved for connection-level frames (PING, PONG, BYE, HELLO, WELCOME, AUTH, AUTH_OK, error frames not associated with a stream).

`stream_id` 1, 3, 5, ... (odd) are client-allocated. `stream_id` 2, 4, 6, ... (even) are reserved for server-initiated streams in the future; not currently used.

#### 3.6 header_crc32c

CRC32C of the header. Computed over bytes 0–7 followed by bytes 12–31 — i.e., the entire header minus the `header_crc32c` field itself. During computation, the `header_crc32c` field is treated as if zero (or omitted).

The polynomial is the [Castagnoli polynomial 0x1EDC6F41](https://en.wikipedia.org/wiki/Cyclic_redundancy_check). Hardware acceleration is available on x86 (SSE 4.2) and ARM (CRC32 extension); see [01.05 Hardware](../01_architecture/05_hardware_and_targets.md) §2.1.

A frame with mismatched `header_crc32c` is treated as corruption: the receiver MUST close the connection with a `BadFrame` error.

#### 3.7 payload_crc32c

CRC32C of the payload. Computed over the payload bytes (after the 32-byte header).

If `payload_len = 0`, `payload_crc32c` MUST also be zero.

A frame with mismatched `payload_crc32c` is corruption: connection close with `BadPayload` error.

#### 3.8 Reserved fields

The 8 bytes at positions 24–31 and the single byte at position 19 are reserved for future use. They MUST be zero in the current wire version. Receivers MUST verify they are zero; non-zero values are protocol errors.

The reserved space provides room for future additions:

- More flags.
- Per-frame priority indicators.
- Tracing IDs (alternatives to OpenTelemetry's W3C trace context).

The exact use is intentionally open; the framing keeps flexibility while remaining stable.

### 4. Frame parsing

#### 4.1 The reader's algorithm

```
loop:
    read 32 bytes (header)
    verify magic == "BRN0"
    verify version matches negotiated version
    verify reserved fields are zero
    verify header_crc32c
    if payload_len > 0:
        read payload_len bytes
        verify payload_crc32c
    dispatch by opcode and stream_id
```

The reader MUST NOT trust any field until the header CRC is verified. Out-of-bounds payload_len, garbage opcodes, etc., are all caught by the header CRC check (assuming the CRC was set correctly by the sender; if the CRC matches but the field is invalid, it's a sender bug, not corruption).

#### 4.2 Why two CRCs

Two checksums seem redundant but serve different purposes:

- **`header_crc32c`** validates the header, so the reader can trust `payload_len` and `payload_crc32c` enough to read the payload.
- **`payload_crc32c`** validates the payload, catching corruption that occurred after the header was written.

A single CRC over both header and payload would require buffering the entire payload before the reader could trust it. Two CRCs let the reader stream-process: parse header, allocate buffer, read payload, validate.

#### 4.3 Why CRC32C, again

Already justified in [`01_design.md`](01_design.md) §7. CRC32C is fast, hardware-accelerated, adequate for transmission-error detection. TLS handles adversarial concerns; CRCs handle accidental corruption.

### 5. Frame size

Minimum frame size: 32 bytes (header only, no payload). Maximum frame size: 32 + 16,777,215 = 16,777,247 bytes.

The 16 MiB limit on payload prevents pathological frames that would block the connection while being read. For larger transfers, multi-payload framing (the `MPL` flag) is used.

### 6. Multi-payload frames

When a logical message exceeds 16 MiB, the sender splits it into multiple frames:

- All frames have the same `stream_id` and `opcode`.
- All but the last have `MPL = 1`.
- The last frame has `MPL = 0` (and may have `EOS = 1` if it's the end of the stream).
- The receiver concatenates the payloads in receive order.

This is rarely needed in practice. Most operations produce small frames; multi-payload kicks in only for very large `RECALL` results (10000+ memories) or large bulk transfers.

### 7. Frame examples

#### 7.1 PING frame

```
Field            Value
magic            "BRN0"
version          1
opcode           0x0010 (PING)
flags            0x00
header_crc32c    <computed>
stream_id        0
payload_len      0
reserved         0
payload_crc32c   0
reserved         0..0
```

32 bytes total. No payload.

#### 7.2 RECALL request frame

```
Field            Value
magic            "BRN0"
version          1
opcode           0x0021 (RECALL_REQ)
flags            0x00
header_crc32c    <computed>
stream_id        7 (client-allocated, odd)
payload_len      <size of CBOR-encoded RecallRequest>
reserved         0
payload_crc32c   <computed>
reserved         0..0
```

Plus the CBOR-encoded RecallRequest payload. See [`05_frame_layouts.md`](05_frame_layouts.md) for layout.

#### 7.3 RECALL response frame (intermediate, with one result)

```
Field            Value
magic            "BRN0"
version          1
opcode           0x00A1 (RECALL_RESP)
flags            0x00  (not EOS yet)
header_crc32c    <computed>
stream_id        7
payload_len      <size of one MemoryResult>
reserved         0
payload_crc32c   <computed>
reserved         0..0
```

#### 7.4 RECALL response frame (final, EOS)

```
Field            Value
magic            "BRN0"
version          1
opcode           0x00A1 (RECALL_RESP)
flags            0x80  (EOS)
header_crc32c    <computed>
stream_id        7
payload_len      0  (or final batch of results)
reserved         0
payload_crc32c   <computed>
reserved         0..0
```

### 8. Endianness summary

| Field | Endianness |
|---|---|
| `magic` | byte order (literal ASCII) |
| `version` | single byte |
| `opcode` | big-endian `u16` |
| `flags` | single byte |
| `header_crc32c` | big-endian `u32` |
| `stream_id` | big-endian `u32` |
| `payload_len` | big-endian `u24` |
| reserved bytes | byte order; must be zero |
| `payload_crc32c` | big-endian `u32` |

Within payloads, encodings (CBOR maps and little-endian f32 vectors) have their own conventions; see the payload encoding section below.

## Payload Encoding

### 9. Two sections, one payload

A single payload may carry both:

- **Structured data** (memory IDs, scores, salience, metadata) — a [CBOR](https://www.rfc-editor.org/rfc/rfc8949) map.
- **Raw vector bytes** (`f32` arrays representing embeddings) — appended after the CBOR map, read as a plain little-endian `f32` array.

The structured part is portable: any language decodes the CBOR map with a stock library, which is what lets Brain ship no client. The raw vector part stays zero-copy: it is a contiguous `f32` array the reader slices directly, no per-element decode. Most payloads carry no vector section at all — clients send text, and Brain owns the embedding model.

### 10. The payload layout

A typical payload:

```
+----------------------------------+
| CBOR-encoded map (N bytes)       |
+----------------------------------+
| raw f32 vectors (M bytes)        |
+----------------------------------+
```

The CBOR map includes fields giving the offset (relative to the start of the payload) and dimension of the raw vector data. This lets the reader find the vector portion without scanning. As a CBOR map, a RECALL request payload is:

```
{
  "cue_text":            <text string>,
  "cue_vector_offset":   <u32: 0 if cue is text-only; byte offset if vector pre-supplied>,
  "cue_vector_dim":      <u16: 0 if no vector; 384 if present>,
  "top_k":               <u32>,
  "confidence_threshold":<f32>,
  "context_filter":      <array of u64, or absent>,
  ...
}
```

After the CBOR map, the raw vector bytes (if any) follow. For payloads with no vector data (the common case), `cue_vector_offset = 0` and the raw section is empty.

Field keys are short strings as shown; an implementation MAY use integer keys for compactness as long as the per-opcode schema in [`05_frame_layouts.md`](05_frame_layouts.md) is followed and the conformance vectors ([§19](../19_benchmarks/00_purpose.md)) match. The reference encoding uses string keys for legibility.

### 11. CBOR

[CBOR](https://www.rfc-editor.org/rfc/rfc8949) (RFC 8949, "Concise Binary Object Representation") is an IETF-standard, self-describing binary format with a small encoder/decoder in every mainstream language.

#### 11.1 What CBOR provides

CBOR is self-describing: the bytes carry their own type tags, so a reader decodes a payload into a map without a schema and without a Brain-supplied library. This is the property that makes "no first-party SDK" honest — a third-party client uses its language's stock CBOR library and reads the documented fields. The cost relative to a zero-copy encoding is a decode pass, which for Brain's small text-bearing payloads is single-digit microseconds.

#### 11.2 Deterministic encoding

Senders MUST use a **reproducible deterministic** encoding: definite-length items, shortest-form (canonical) integer encoding, and a fixed field order per payload type (the order the per-opcode field schema lists). A given logical payload therefore encodes to one exact byte sequence — which is what lets the conformance corpus ([§19](../19_benchmarks/00_purpose.md)) ship golden bytes, and what keeps request-hash idempotency stable across clients. This is reproducibility from a fixed schema, not the full key-sorted canonical profile of RFC 8949 §4.2.1: keys appear in schema order, not bytewise-sorted order. The golden corpus pins the exact bytes regardless, so conformance is byte-exact either way.

#### 11.3 Version pinning

The CBOR data model is stable (RFC 8949 obsoletes RFC 7049 without breaking the core encoding). The wire-protocol version field (§3.2) covers the *schema* of each payload — which fields exist for each opcode — not the CBOR encoding itself. Adding or changing a field in an opcode's schema bumps the wire version.

#### 11.4 Validation

Receivers MUST validate every payload before acting on it: the CBOR MUST be well-formed (RFC 8949 §5.3.1), MUST decode to the map shape the opcode's schema specifies, and MUST NOT carry unknown keys. A payload that fails any of these is a protocol error (`MalformedPayload`), not garbage to be best-effort-parsed. The validation cost is small and is paid on every frame.

### 12. The raw vector section

The raw vector portion of a payload is a sequence of `f32` values, each 4 bytes, read directly from the network buffer as an `f32` array — no decode loop, no per-element overhead.

#### 12.1 Endianness for vectors

Vectors use **little-endian** `f32`, matching the byte layout of common CPUs (x86 and ARM). Big-endian would force a byte swap on the hot path, which the protocol avoids. This is the one place the protocol deviates from the big-endian rule that governs the header and CBOR integers; the deviation is deliberate and documented so a client byte-swaps correctly on a big-endian host.

#### 12.2 Alignment

`f32` alignment is 4 bytes on all target architectures. The CBOR portion of the payload may end at any byte boundary; it is padded with zero bytes to a multiple of 4 so the vector portion starts aligned. The padding length (0 to 3 bytes) is `(4 - (cbor_len % 4)) % 4`; the reader computes the vector offset from the CBOR map's `vector_offset` field, which already accounts for the padding.

#### 12.3 Multiple vectors per payload

For payloads with multiple vectors (e.g., `RECALL` results carrying multiple memory vectors when the client asked for them), the vectors are concatenated in the raw section. The CBOR map indexes into the raw section via offset and dim per vector:

```
+-----------------------+
| CBOR map              |
|   results: [          |
|     { vec_offset: 0,   dim: 384 }   |
|     { vec_offset: 1536, dim: 384 }  |
|     ...               |
|   ]                   |
+-----------------------+
| padding (0-3 bytes)   |
+-----------------------+
| f32 vector 0 (1536 b) |
+-----------------------+
| f32 vector 1 (1536 b) |
+-----------------------+
| ...                   |
+-----------------------+
```

Vectors are typically same-size (384 dims = 1536 bytes). Mixed-size vectors are supported via the explicit per-vector dim but unusual.

### 13. The full payload read algorithm

A frame's payload is:

```
[CBOR map]  [padding (0-3 bytes)]  [raw vector bytes]
```

The reader:

1. Reads the entire payload (`payload_len` bytes).
2. Validates `payload_crc32c`.
3. Decodes the leading CBOR map (a CBOR decoder consumes exactly the map's bytes and reports where it ended).
4. Reads the `vector_offset` / `vector_dim` fields from the map.
5. If a vector is present, slices the raw section at `vector_offset` as a little-endian `f32` array of length `vector_dim`.

### 14. Why not put vectors in the CBOR map

The structured part is CBOR. Why not put vectors in the CBOR map too, as an array of floats?

A 384-dim vector encoded as a CBOR array is 384 tagged float items — per-element tag overhead, and a decode loop the receiver pays even when it only wants to forward the bytes. The raw trailing section bypasses both: the vectors are a contiguous `f32` array, sliced in one step.

The split: structured data goes through CBOR (portable, self-describing), bulk vector data goes in the raw section (zero-copy, no per-element cost). Each part uses the representation that fits it.

### 15. ENCODE_VECTOR_DIRECT payload

`ENCODE_VECTOR_DIRECT` is the power-user opcode that lets clients send pre-computed vectors. Its CBOR map:

```
{
  "text":              <text string>,
  "vector_offset":     <u32: offset to the vector in the raw section>,
  "vector_dim":        <u16: expected 384>,
  "model_fingerprint": <16-byte string>,
  "context_id":        <u64>,
  "salience_hint":     <f32>,
  "request_id":        <16-byte string>
}
```

Followed by the raw vector. The model fingerprint identifies which embedding model produced the vector; the server validates that fingerprint matches the shard's loaded model. This is the only common path that carries a raw vector section on a *request*.

### 16. RECALL request payload

A request without a pre-supplied vector (the common case) is the map shown in §10 with `cue_vector_offset = 0`, `cue_vector_dim = 0`, and no raw section. A request with a pre-supplied cue vector sets `cue_vector_offset` to the (post-padding) byte offset and `cue_vector_dim = 384`, followed by 1536 bytes of little-endian `f32`.

The full field list (types, required/optional, sentinels) for RECALL_REQ and every other opcode is in [`05_frame_layouts.md`](05_frame_layouts.md).

### 17. RECALL response payload

Each response frame carries a batch of results as a CBOR map:

```
{
  "is_final":          <bool: matches the EOS flag, redundantly>,
  "results": [
    {
      "memory_id":        <16-byte string>,
      "text":             <text string>,
      "similarity_score": <f32>,
      "confidence":       <f32>,
      "salience":         <f32>,
      "kind":             <u8 enum>,
      "context_id":       <u64>,
      "created_at":       <u64>,
      "vector_offset":    <u32: offset into raw section if vector included, else 0>,
      "vector_dim":       <u16: 384 if included, else 0>
    },
    ...
  ]
}
```

Followed by 0 or more vectors in the raw section. By default vectors are NOT included in `RECALL` responses (`vector_offset = 0`); the `include_vectors` request flag enables them.

### 18. Compression

The frame header reserves a `CMP` flag for compression. Not currently used.

If a future major version adds compression (probably zstd over the CBOR section), the flag is set and the receiver decompresses before decoding. For the current wire version, all payloads are uncompressed and a set `CMP` bit is a protocol error.

### 19. Payload size estimation

A typical encode payload:

- CBOR map: ~150–250 bytes (text varies)
- vector data: 0 (text-only encode) or 1536 bytes (vector pre-supplied)
- total: ~150–2000 bytes

A typical recall request:

- CBOR map: ~80–180 bytes (cue text varies)
- vector data: 0 (typical)
- total: ~80–180 bytes

A typical recall response (10 memories, no vectors):

- CBOR map: ~2–3 KiB (10 × ~250 bytes per result)
- vector data: 0
- total: ~2–3 KiB

A recall response with vectors (10 memories, vectors included):

- CBOR map: ~2–3 KiB
- vector data: 10 × 1536 = 15,360 bytes
- total: ~18 KiB

CBOR is slightly larger on the wire than a packed zero-copy encoding (self-describing tags cost a byte or two per field), but for Brain's text-dominated payloads the difference is noise against the text itself. All sizes are well within the 16 MiB single-frame limit; multi-payload framing is reserved for unusual cases.

### 20. Typed-graph payload conventions

The `0x01xx` (typed-graph) opcodes reuse the payload encoding rules: a CBOR map (deterministic profile, validated), big-endian multi-byte integers, CRC32C over the body, 16 MiB − 1 hard cap, `MPL` for multi-frame logical payloads. This section documents the typed-graph-specific conventions for what goes *inside* a CBOR body.

#### 20.1 Opaque blob fields

Several typed-graph payloads carry byte-string fields that the wire layer does not interpret:

| Field | Carrier | Schema-aware decode by |
|---|---|---|
| `EntityCreateRequest.attributes_blob` | entity ops | the schema validator |
| `EntityView.attributes_blob` | entity reads | the client |
| `RelationCreateRequest.properties_blob` | relation ops | the schema validator |
| `RelationView.properties_blob` | relation reads | the client |
| `StatementValueWire::Blob(_)` (inner) | statement values | application-level |
| `SchemaUploadRequest.schema_document` (text) | schema upload | parser |

##### 20.1.1 Inner encoding

`attributes_blob` and `properties_blob` are themselves **nested CBOR maps** carried as a CBOR byte string in the outer payload:

```
// Logical shape (decoded after schema validation):
{ attribute-name (string): value }
```

The wire layer treats the blob as opaque bytes. The schema validator decodes the nested CBOR map and validates it against the entity/relation type's declared attribute schema before the redb commit. Validation failures surface as `EntityTypeMismatch` (schema-aware).

##### 20.1.2 Why a nested blob (not flattened fields)

The wire layer can't flatten attributes into top-level keys because it doesn't know any user-defined type's attribute schema (types are declared at runtime via SCHEMA_UPLOAD). Carrying the attributes as an opaque nested-CBOR blob defers interpretation to the handler, which has the schema. It's the same CBOR codec for inner and outer — one codec, one validation pass, and a third-party client builds the blob with the same library it uses for the rest of the frame.

##### 20.1.3 Size cap

Each opaque blob ≤ 64 KiB (see [`07_error_handling.md`](07_error_handling.md) §16.9.1). Above that, callers split the payload into auxiliary records (statements with `EvidenceRef::Overflow`, etc.) or use a future "BLOB_PUT" pathway.

#### 20.2 Evidence encoding

Statements carry an `EvidenceRefWire`:

```rust
pub enum EvidenceRefWire {
    Inline(Vec<u128>),     // ≤ 8 MemoryIds
    Overflow(WireUuid),    // EvidenceOverflowId
}
```

**Inline path.** `Inline` carries up to 8 packed `MemoryId`s as `u128` values (the same packing used in substrate ops). Cheap and zero-decode. Reject on the server if `len > 8`.

**Overflow path.** For larger evidence sets, the client pre-creates an `evidence_overflow` row out-of-band (via a future `EVIDENCE_PUT` opcode; today only the worker pipeline writes these rows) and references its UUIDv7 in `EvidenceRef::Overflow`. Reads dereference transparently.

**No middle ground.** There's deliberately no "13 inline" or "32 inline" tier. Either ≤ 8 (the common case for hand-authored statements) or overflow. Avoids tier-boundary edge cases in the storage layer.

#### 20.3 Predicate strings

`predicate` fields are wire-carried as `String` in their canonical `"namespace:name"` form. The server interns them into a `predicates` redb table on first encounter (per [`../02_data_model/00_purpose.md`](../02_data_model/00_purpose.md)). Subsequent reads emit the same canonical form back to the wire.

**Why strings, not interned `u32`?** Convenience for clients. A client constructing a `StatementCreateRequest` shouldn't have to look up a `PredicateId` first. The intern step happens server-side on the create path.

**Trade-off.** ~20-40 bytes per statement frame for the predicate string vs ~4 bytes for an interned id. Acceptable for current scale; revisit if `STATEMENT_LIST` streaming becomes bandwidth-bound at high QPS.

#### 20.4 Time fields

All time fields are **unix nanoseconds**, `u64`.

**Sentinel zero for "absent".** `valid_from_unix_nanos = 0`, `valid_to_unix_nanos = 0`, `event_at_unix_nanos = 0` mean "absent / not applicable" — not "January 1, 1970 00:00:00 UTC". Anyone encoding the unix epoch literally should encode as `1` ns instead (or accept the loss of one ns precision).

**Why not `Option<u64>`?** Same reasoning as [§01/§12](01_design.md) — a CBOR-level optional is avoided here; sentinel zero is the wire convention. Sentinel zero is simpler. Documented per-field where it matters.

#### 20.5 Pagination cursors

`ENTITY_LIST`, `STATEMENT_LIST`, `RELATION_LIST_*`, `SCHEMA_LIST`, `EXTRACTOR_LIST` all carry an opaque `Vec<u8>` cursor field for continuation. The shape is server-defined:

```rust
// Currently : opaque cursor blob containing the last seen key in the
// query's primary index. Concretely for ENTITY_LIST it's the last EntityId scanned,
// for STATEMENT_LIST it's a (subject, predicate, statement_id) triple, etc.
```

**Why opaque?** Clients shouldn't depend on the cursor's internal shape — it's free to change between phases as indexes evolve. The wire shape is just "give me back what the server gave you, and you'll get the next page".

**Cap.** ≤ 1 KiB. Malformed cursors (e.g. an `ENTITY_LIST` cursor fed to `STATEMENT_LIST`) error out with `InvalidArgument`.

**Stability across schema changes.** A cursor issued under schema version N is not guaranteed valid under schema version N+1. Clients that span a `SCHEMA_UPDATED` event should restart their list scan from cursor `Vec::new()`.

#### 20.6 Sentinel and reserved fields summary

For implementers, the complete list of "sentinel zero means absent" fields in typed-graph bodies:

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

Reserved `u8` / `u32` byte / word fields that MUST be zero: none in typed-graph bodies (the frame header carries the only reserved bytes; see §1 above).

#### 20.7 Large-blob policy

Typed-graph bodies carry blobs ≤ 64 KiB inline. Blobs ≥ 64 KiB use:

- **For evidence:** `EvidenceRef::Overflow(EvidenceOverflowId)` referencing a separate `evidence_overflow` row.
- **For other large payloads** (e.g. an embedding blob): currently no typed-graph opcode carries one directly; future ops may use a "BLOB_PUT then reference by id" pattern.

The 64 KiB cap is per-blob, not per-frame. A frame may carry multiple ≤ 64 KiB blobs as long as the total fits in the 16 MiB - 1 frame budget. Multi-payload framing (the `MPL` flag) extends beyond a single frame's budget if needed.

---

*Continue to [`03_opcodes.md`](03_opcodes.md) for the full opcode table.*
