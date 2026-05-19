# 03.04 Payload Encoding

The frame header is followed by a payload. This file specifies how payloads are encoded — the rkyv-for-structured-data plus bytemuck-for-raw-vectors split.

## 1. Two encodings, one payload

A single payload may carry both:

- **Structured data** (memory IDs, scores, salience, metadata) — encoded with [rkyv](https://github.com/rkyv/rkyv).
- **Raw vector bytes** (`f32` arrays representing embeddings) — appended after the rkyv data, accessed via [bytemuck](https://github.com/Lokathor/bytemuck).

The split lets us achieve zero-copy reads on both components. rkyv's structured access works on the rkyv portion; bytemuck's `cast_slice<u8, f32>` on the trailing bytes gives direct access to the vector data without decoding.

## 2. The payload layout

A typical payload:

```
+----------------------------------+
| rkyv-encoded structure (N bytes) |
+----------------------------------+
| raw f32 vectors (M bytes)        |
+----------------------------------+
```

The rkyv structure includes a field giving the offset (relative to the start of the payload) and length of the raw vector data. This lets the reader find the vector portion without scanning.

```rust
#[derive(rkyv::Archive)]
struct RecallRequestPayload {
    cue_text: String,
    cue_vector_offset: u32,    // 0 if cue is text-only; non-zero if vector pre-supplied
    cue_vector_dim: u16,        // 0 if no vector; 384 if present
    top_k: u32,
    confidence_threshold: f32,
    context_filter: Option<ContextFilter>,
    // ... other fields
}
```

After the rkyv data, the raw vector bytes (if any) follow.

For payloads with no vector data (e.g., metadata-only requests), `cue_vector_offset = 0` and the raw section is empty.

## 3. rkyv

[rkyv](https://github.com/rkyv/rkyv) is a Rust serialization framework providing zero-copy deserialization. Self-described as "a zero-copy deserialization framework for Rust."

### 3.1 What rkyv gives us

rkyv lets the receiver access deserialized data without copying it. The bytes on the wire are the same bytes the program reads as a struct (modulo a small header). For a 1 KiB structured payload with many fields, this saves the copy and the per-field decode that Protobuf would perform.

The trade-off is that the on-wire format is rkyv-specific — third-party readers (without an rkyv library) can't easily decode it. We accept this; SDKs include rkyv.

### 3.2 rkyv version pinning

The protocol pins to a specific rkyv version (initially 0.7.x). Within a major rkyv version, format compatibility is preserved. Bumping rkyv to a new major version requires bumping the wire-protocol version.

### 3.3 What rkyv-encoded data looks like

rkyv writes the data structure followed by a small trailer (the "root pointer") that lets the reader find the start of the encoded structure within the buffer. The reader decodes by:

1. Reading the buffer.
2. Asking rkyv for the archived view at the buffer's end.
3. Accessing fields via the archived view; no allocation, no copy.

For our purposes, the buffer is the rkyv portion of the payload. The reader knows the rkyv portion's size from the rkyv root pointer (and from the difference between `payload_len` and the trailing vector bytes).

### 3.4 Validation

rkyv supports validation: checking that an archive is well-formed before accessing it. We use rkyv's validation in v1 to catch malformed data (which would be a protocol error, not just garbage data).

The validation cost is small (~100 ns for typical payloads) and worth paying for safety.

## 4. bytemuck

[bytemuck](https://github.com/Lokathor/bytemuck) provides safe bit-cast operations between types of compatible memory layout.

### 4.1 What we use it for

The raw vector portion of a payload is a sequence of `f32` values, each 4 bytes. bytemuck's `cast_slice<u8, f32>` reinterprets the byte slice as an `f32` slice without copying:

```rust
let vector_bytes: &[u8] = &payload[vector_offset..vector_offset + vector_byte_len];
let vector: &[f32] = bytemuck::cast_slice(vector_bytes);
```

The reader gets a `&[f32]` directly into the network buffer. No allocation, no decode loop, no per-element overhead.

### 4.2 Endianness for vectors

Vectors use **little-endian** `f32`, matching the byte layout of common CPUs (x86 and ARM). This is a conscious choice — using big-endian would force a byte swap on the hot path, which we want to avoid.

The endianness mismatch with the frame header (big-endian) is internally consistent within the protocol's frame: header in big-endian, structured payload data in rkyv's native (little-endian on most platforms), vector bytes in little-endian. The reader handles each section appropriately.

### 4.3 Alignment

`f32` alignment is 4 bytes on all architectures we target. The rkyv portion of the payload may end at any byte boundary; we pad the rkyv portion to a multiple of 4 bytes so the vector portion starts aligned.

The padding bytes are zero. The padding length (0 to 3 bytes) is determined by the rkyv portion's size; the reader skips the padding to reach the vector portion.

### 4.4 Multiple vectors per payload

For payloads with multiple vectors (e.g., `RECALL` results carrying multiple memory vectors), the vectors are concatenated in the raw section. The structured section indexes into the raw section via offset and length per vector.

```
+-----------------------+
| rkyv portion          |
|   results: [          |
|     { vec_offset: 0,  |
|       vec_len: 1536 } |
|     { vec_offset: 1536|
|       vec_len: 1536 } |
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

Vectors are typically same-size (384 dims = 1536 bytes). Mixed-size vectors are supported via the explicit length field but unusual.

## 5. The full payload format

A frame's payload has the structure:

```
[rkyv-encoded header]  [rkyv root pointer]  [padding]  [raw vector bytes]
```

The rkyv root pointer is at the end of the rkyv portion (rkyv's convention). The reader:

1. Reads the entire payload (`payload_len` bytes).
2. Validates `payload_crc32c`.
3. Locates the rkyv root pointer at offset N (where N is set by rkyv's encoding).
4. Accesses the archived structure, including the `vector_offset` and `vector_len` fields.
5. Casts the raw section to `&[f32]` via bytemuck.

## 6. Why not just rkyv for everything

We use rkyv for the structured part. Why not put vectors in the rkyv structure too?

A vector field in rkyv would be encoded element-by-element. For 384-dim `f32` vectors, that's 384 fields per vector with rkyv overhead per field. Even with rkyv's efficient encoding, the overhead is non-trivial.

Putting vectors in the raw section bypasses this. The vectors are just bytes; the reader gets a slice without overhead.

The architectural symmetry: structured data goes through rkyv (whose strength is structured access), bulk data goes through bytemuck (whose strength is bulk byte access). Each tool for its own job.

## 7. Encoding the cue vector for ENCODE_VECTOR_DIRECT

`ENCODE_VECTOR_DIRECT` is the power-user opcode that lets clients send pre-computed vectors. Its payload:

```rust
struct EncodeVectorDirectPayload {
    text: String,
    vector_offset: u32,         // offset to the vector in the raw section
    vector_dim: u16,            // expected: 384
    model_fingerprint: [u8; 16],
    context_id: ContextId,
    salience_hint: f32,
    request_id: RequestId,
}
```

Followed by the raw vector. The model fingerprint identifies which embedding model produced the vector; the server validates that fingerprint matches a known model.

## 8. Encoding RECALL request

A request without a pre-supplied vector:

```rust
struct RecallRequestPayload {
    cue_text: String,
    cue_vector_offset: u32,     // 0 — no pre-supplied vector
    cue_vector_dim: u16,         // 0
    top_k: u32,
    confidence_threshold: f32,
    context_filter: Option<Vec<ContextId>>,
    age_bound_unix_nanos: Option<u64>,
    kind_filter: Option<Vec<MemoryKind>>,
    strategy_hint: Option<RecallStrategy>,
    request_id: Option<RequestId>,
}
```

`RecallStrategy` is encoded as a `u8` tag: `Auto = 0`, `SubstrateOnly = 1`, `HybridOnly = 2`. See [`07_request_frames.md`](07_request_frames.md) §3 for semantics.

Followed by no raw vector data.

A request with pre-supplied cue vector:

```rust
struct RecallRequestPayload { ... cue_vector_offset: <set> ... cue_vector_dim: 384 ... }
```

Followed by 1536 bytes of vector data.

## 9. Encoding RECALL response

Each response frame carries one result (or a small batch):

```rust
struct RecallResponsePayload {
    is_final: bool,             // matches the EOS flag, redundantly
    results: Vec<MemoryResult>,
}

struct MemoryResult {
    memory_id: MemoryId,
    text: String,
    similarity_score: f32,
    confidence: f32,
    salience: f32,
    kind: MemoryKind,
    context_id: ContextId,
    created_at: u64,
    vector_offset: u32,         // offset into raw section, if vector included
    vector_dim: u16,             // 384 if included, 0 otherwise
}
```

Followed by 0 or more vectors in the raw section.

By default, vectors are NOT included in `RECALL` responses. Clients receive `vector_offset = 0` and have no vectors. The `include_vectors` flag in the request enables vector return; when set, each result's vector is included in the raw section.

## 10. Compression

The frame header reserves a `CMP` flag for compression. Not used in v1.

If a future version adds compression (probably zstd over the entire payload, or just the rkyv portion), the flag is set and the receiver decompresses before parsing.

For v1, all payloads are uncompressed.

## 11. Payload size estimation

A typical encode payload:

- rkyv structure: ~150–200 bytes (text varies)
- vector data: 0 (text-only encode) or 1536 bytes (vector pre-supplied)
- total: ~150–2000 bytes

A typical recall request:

- rkyv structure: ~80–150 bytes (cue text varies)
- vector data: 0 (typical)
- total: ~80–150 bytes

A typical recall response (10 memories, no vectors):

- rkyv structure: ~2 KiB (10 × ~200 bytes per result)
- vector data: 0
- total: ~2 KiB

A recall response with vectors (10 memories, vectors included):

- rkyv structure: ~2 KiB
- vector data: 10 × 1536 = 15,360 bytes
- total: ~17 KiB

These are well within the 16 MiB single-frame limit. Multi-payload framing is reserved for unusual cases.

---

*Continue to [`05_opcodes.md`](05_opcodes.md) for the full opcode table.*
