# 03.11 Frame and Payload Validation

What the server validates when it receives a frame, and what it rejects. This file defines the substrate's defensive posture: the protocol assumes adversarial input and validates aggressively.

The validation rules here MUST be implemented by every conforming server. SDKs SHOULD perform the same validation client-side to fail fast on bugs without round-tripping to the server.

## 1. Layered validation

Validation happens in three layers, in order:

1. **Frame-level** — the 32-byte header is parsed and structurally validated. Bad framing closes the connection.
2. **Payload-level** — the payload is decoded (rkyv or bytemuck) and structurally validated. Bad payloads return an error frame.
3. **Operation-level** — the parameters of a specific opcode are validated against the data model. Bad parameters return an opcode-specific error frame.

Earlier failures take precedence over later ones. If the frame's header is malformed, the payload is never decoded.

## 2. Frame-level validation

For every incoming frame, the server checks:

### 2.1 Magic bytes

The first four bytes MUST be `BRN0` (0x42, 0x52, 0x4E, 0x30). Any deviation closes the connection without sending an error — the peer is not speaking Brain.

**Rationale.** Magic-byte mismatch usually means the connection got garbled (TLS misconfiguration, port confusion). Returning an error frame would imply we're a Brain server willing to talk; closing silently is the safer response to "this is not Brain traffic".

### 2.2 Version field

The version field MUST be a wire-protocol version the server supports (currently 1). If the version is unknown:

- During handshake (before WELCOME): the server returns `WireVersionNegotiationFailed` and closes.
- After handshake: the server returns `BadFrame` and closes — receiving an unexpected version after handshake means client/server got out of sync.

### 2.3 Header CRC32C

The header carries a CRC32C of bytes [0..28]. The server recomputes and compares. Mismatch closes the connection — header corruption typically means deeper transport corruption that further communication cannot recover from.

### 2.4 Payload length bounds

The 24-bit `payload_len` MUST be ≤ 16 MiB (2^24 - 1). Larger values close the connection.

The server's *effective* limit is configurable, defaulting to 16 MiB. A frame with `payload_len > effective_limit` returns `PayloadTooLarge` and closes the stream (but not the connection, unless many such frames arrive).

### 2.5 Stream ID rules

Stream IDs MUST follow the parity convention from [`05_streams.md`](05_streams.md):

- Client-initiated streams: odd values.
- Reserved for server-initiated: even values (not used in v1).
- Stream ID 0: reserved for connection-level frames (HELLO, WELCOME, PING, PONG, BYE).

Violations return `BadFrame`.

### 2.6 Opcode validity

The opcode MUST be a known value. Unknown opcodes return `UnknownOpcode`. The connection stays open; the client can send other valid opcodes.

The set of valid opcodes is defined in [`05_opcodes.md`](05_opcodes.md). Adding a new opcode is a wire-version bump.

## 3. Payload-level validation

After the frame header is accepted, the payload is decoded.

### 3.1 Payload CRC32C

The payload's CRC32C in the header is recomputed against the actual payload bytes. Mismatch returns `PayloadCorrupted`.

For zero-length payloads, the payload CRC field is 0; no CRC check.

### 3.2 rkyv structural validation

For structured payloads encoded with rkyv:

- The payload MUST decode without error using rkyv's `check_archived_root` (or equivalent validation API). This catches:
  - Truncated payloads.
  - Out-of-bounds offsets within the payload.
  - Type tag mismatches.
- Decoded values MUST satisfy any per-field constraints documented in the opcode's request shape (see [`07_request_frames.md`](07_request_frames.md)).

Failure returns `BadPayload`.

### 3.3 bytemuck raw payloads

For raw byte payloads (vectors via `ENCODE_VECTOR_DIRECT`):

- The payload length MUST exactly equal `dim * sizeof(f32)` (1536 bytes for our 384-dim vector).
- Length mismatch returns `BadPayload`.

After length check, the bytes are interpreted as `[f32; 384]`. Element-level validation (NaN, Inf, normalization) is a separate operation-level check.

## 4. Operation-level validation

Per-opcode validation, after the payload has decoded successfully.

### 4.1 ENCODE

- `text` MUST be valid UTF-8.
- `text` MUST NOT be empty after Unicode whitespace trim.
- `text.len()` MUST be ≤ `max_text_bytes` (default 1 MiB, configurable).
- `request_id` MUST NOT be the all-zero `RequestId`.
- `salience_hint`, if present, MUST be in [-1.0, +1.0]. NaN/Inf rejected.
- `context_id` MUST exist for the agent's namespace (or the operation specifies a context name to lazy-create).
- `kind`, if explicitly set, MUST be a valid `MemoryKind`. Server may reject `kind = Consolidated` since clients are forbidden from creating consolidated memories.
- Outgoing edges in the same payload MUST reference existing memories of the same agent. Cross-agent or non-existent targets return `InvalidArgument`.

### 4.2 ENCODE_VECTOR_DIRECT

All ENCODE rules plus:

- The vector MUST contain no NaN or Inf elements. Failing element returns `InvalidVector`.
- The vector's L2 norm MUST be in `[1.0 - 1e-3, 1.0 + 1e-3]`. Out-of-range returns `InvalidVector`.
- The supplied `embedding_model_fp` MUST match a model the server knows about. Unknown fingerprint returns `UnknownModel`.

### 4.3 RECALL

- `cue_text` validation matches `ENCODE`'s `text`.
- `top_k` MUST be in `[1, 1000]`. Higher values capped to 1000.
- `confidence_min`, if present, MUST be in [0.0, 1.0]. NaN/Inf rejected.
- `age_bound`, if present, MUST be a non-negative duration.
- `context_filter`, if present, MUST reference contexts that exist for the agent. Unknown contexts return `InvalidContext`.
- `kind_filter`, if present, MUST contain only valid `MemoryKind` values.

### 4.4 PLAN, REASON

- `start_state` and `goal_state` (for PLAN) MUST be either valid `MemoryId`s or non-empty text.
- `MemoryId` references MUST belong to the agent.
- `budget` MUST have at least one dimension specified (max steps, max wall time, or max branches). At least one bound is required to prevent unbounded search.

### 4.5 FORGET

- `memory_id` MUST be a valid `MemoryId` belonging to the agent.
- `request_id` MUST NOT be the all-zero value.
- `mode` MUST be either `Soft` or `Hard`.

### 4.6 SUBSCRIBE

- `filter` parameters validated per their types.
- `from_lsn`, if present, MUST be ≤ the server's current LSN. Future LSNs return `InvalidArgument`.
- `from_lsn` older than the WAL retention horizon returns `LsnTooOld`.

### 4.7 TXN_*

- `txn_id` MUST be a non-zero `TxnId`.
- `TXN_BEGIN` for an already-active `txn_id` returns `TransactionExists`.
- `TXN_COMMIT` and `TXN_ABORT` for unknown `txn_id` return `UnknownTransaction`.
- Operations within a transaction MUST carry the matching `txn_id`. Mismatched ids return `WrongTransaction`.

### 4.8 ADMIN_*

- The session MUST have admin privileges (set during AUTH). Otherwise `Unauthorized`.
- Per-admin-opcode parameter validation as documented in the opcode's request shape.

## 5. Cross-frame validation

Some validations span multiple frames.

### 5.1 Stream lifecycle

Frames within a stream MUST follow the lifecycle rules from [`05_streams.md`](05_streams.md):

- A stream is opened by the first request frame.
- Subsequent frames on the stream MUST be either continuation or response frames belonging to that opcode's pattern.
- A stream is closed by the end-of-stream frame.
- Frames on a closed stream are rejected with `BadStream`.

### 5.2 Handshake order

Operations are rejected with `NotHandshaked` until WELCOME has been received.
Operations requiring authentication are rejected with `NotAuthenticated` until AUTH_OK has been received.

### 5.3 Idempotency consistency

If the same `RequestId` is reused with different parameters (different `text`, different `context_id`, etc.), the server returns `IdempotencyConflict`.

If the same `RequestId` is reused with identical parameters, the server replays the original response.

### 5.4 Transactional ordering

Operations within a transaction are validated as if the transaction's prior operations have been applied. For example, an `ENCODE` followed by a `LINK` referencing the encoded memory's id is validated only at commit; until then, the link is buffered.

If validation fails at commit, the entire transaction is aborted with `TransactionValidationFailed` and a description of which operation failed and why.

## 6. Rate and resource limits

Validation also enforces resource limits (server-side configurable):

| Limit | Default | Error code |
|---|---|---|
| Max in-flight streams per connection | 256 | `TooManyStreams` |
| Max concurrent transactions per session | 16 | `TooManyTransactions` |
| Max operations per transaction | 1000 | `TransactionTooLarge` |
| Max transaction wall time | 60 s | `TransactionTimeout` |
| Max edges per ENCODE | 64 | `TooManyEdges` |
| Max contexts per agent | 65,535 | `TooManyContexts` |
| Frames per second per connection | 10,000 | `RateLimited` |

These are not strict invariants of the protocol; servers MAY configure higher or lower values. Clients SHOULD treat the error codes as recoverable and back off.

## 7. Validation determinism

Validation MUST be deterministic for a given input. The same payload, against the same configuration, MUST always produce the same accept/reject decision.

This matters for testing and debugging: a frame that fails validation in production should fail validation in a developer's reproduction. Non-deterministic validation (e.g., based on memory pressure or time) is forbidden — resource-pressure responses are handled at a different layer.

## 8. The "fail closed" principle

When a validation rule is unclear or under-specified, the server MUST reject. New, unspecified fields, novel opcode parameters, ambiguous flag combinations: all reject by default.

This is the opposite of "best effort". The protocol's premise is that the SDK provides correct frames; anything unrecognized is treated as a bug or attack, not as a feature to be inferred.

## 9. Validation error reporting

Validation errors return error frames containing:

- The error code (one of the values in [`10_errors.md`](10_errors.md)).
- A human-readable message describing what failed.
- The stream ID of the failing frame.
- For payload validation, the offset within the payload where validation failed (best-effort; not always available).

The server SHOULD include enough detail to diagnose the issue without leaking internal state. For example: "field `top_k` out of range; got 5000, max 1000" is fine; "validation failed at internal token 0xDEADBEEF" is not.

## 10. The validation budget

Validation is on the hot path. Each validation step adds latency. The substrate's design budgets validation cost as follows:

- Frame-level: < 100 ns per frame (CRC plus a few comparisons).
- Payload-level rkyv decode: < 1 µs typical, larger for bigger payloads.
- Payload-level bytemuck cast: < 100 ns (no copying, just a length check and a pointer reinterpret).
- Operation-level: < 5 µs typical.

Validators MUST stay within these budgets. Heavyweight checks (e.g., re-embedding to verify a vector) belong in operation handlers, not validation.

---

*Continue to [`12_versioning.md`](12_versioning.md) for version negotiation in detail.*
