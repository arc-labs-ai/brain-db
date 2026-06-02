# 04.07 Error Handling

Error handling spans two layers: error codes / frame layout / propagation (covered first), and the validation rules the server applies on every incoming frame (covered second).

## Errors

### 1. The ERROR frame

Every error is delivered as an `ERROR` frame (opcode `0xFF`). The error frame layout is in [`05_frame_layouts.md`](05_frame_layouts.md) §50.

An error frame has the same `stream_id` as the operation that errored — except for connection-level errors (during handshake, on stream_id 0), which use stream_id 0.

An error frame implicitly carries EOS for the stream it terminates. After receiving an error, the client SHOULD NOT expect any further frames on that stream.

### 2. Error categories

The `ErrorCategory` enum classifies errors into broad groups:

| Category | Meaning | Retryable? |
|---|---|---|
| `Protocol` | Bad frame, version mismatch, malformed message | No (client bug) |
| `Authentication` | Auth failed | Maybe (refresh credentials and retry) |
| `Authorization` | Permission denied | No |
| `Validation` | Invalid argument | No (fix the request) |
| `NotFound` | Referenced entity doesn't exist | No |
| `Conflict` | Idempotency conflict, transaction conflict | No (semantic conflict) |
| `ResourceExhausted` | Out of slots, disk, rate limit | Yes (after retry-after) |
| `Internal` | Server bug | Maybe (retry; report if persistent) |
| `Unavailable` | Shard unavailable, server overloaded | Yes (after retry-after) |

The category drives client retry behavior. The client uses category to decide whether to retry, propagate, or fail fast.

### 3. Error codes

A more specific error code accompanies each category. The complete table:

#### 3.1 Protocol errors (Category: `Protocol`)

| Code | Meaning |
|---|---|
| `BadMagic` | Frame's magic bytes aren't "BRN0" |
| `BadHeaderCrc` | Header CRC32C doesn't match |
| `BadPayloadCrc` | Payload CRC32C doesn't match |
| `BadOpcode` | Unknown or wrong-direction opcode |
| `BadVersion` | Frame's version doesn't match negotiated version |
| `BadFrame` | Generic malformed frame |
| `OversizePayload` | Payload exceeds server's max |
| `ReservedFieldNonZero` | A reserved field had a non-zero value |
| `BadFlagCombination` | Frame flags are mutually inconsistent |
| `MalformedPayload` | CBOR payload didn't decode or failed schema validation |
| `MalformedVector` | Raw vector bytes don't match declared dim or fail norm check |

#### 3.2 Connection / handshake (Category: `Protocol` or `Authentication`)

| Code | Meaning |
|---|---|
| `VersionNotSupported` | No mutual version between client and server |
| `NoSuchAuthMethod` | AUTH method not in WELCOME's auth_methods |
| `Unauthenticated` | AUTH credentials rejected |
| `NotAuthenticated` | Operation attempted before AUTH_OK |
| `AuthBackendUnavailable` | Auth backend (e.g., token service) unreachable |
| `SessionExpired` | Session timed out (rare; sessions are connection-lifetime) |

#### 3.3 Authorization (Category: `Authorization`)

| Code | Meaning |
|---|---|
| `PermissionDenied` | Agent lacks permission for this operation |
| `AdminPermissionRequired` | Operation requires `can_admin` |
| `WrongShard` | Operation tried to address a different shard than the connection's |

#### 3.4 Validation (Category: `Validation`)

| Code | Meaning |
|---|---|
| `InvalidArgument` | A request field was invalid (details in `ErrorDetails.field`) |
| `MissingRequiredField` | A required field was absent |
| `TextTooLarge` | Encoded text exceeds max |
| `TextEmpty` | Encoded text was empty |
| `BadContextId` | Context ID isn't valid for this agent |
| `BadMemoryKind` | Invalid memory kind (e.g., client-supplied `Consolidated`) |
| `BadEdgeKind` | Unknown edge kind |
| `BadStrategyHint` | Strategy hint not valid for this operation |
| `TopKOutOfRange` | top_k exceeds limit |
| `BudgetTooLarge` | Plan/reason budget exceeds limit |
| `BadModelFingerprint` | Model fingerprint doesn't match a known model |
| `PredicateNotInSchema` (0x004B) | Predicate qname is not declared in the active schema for this namespace. Returned in strict mode only. |
| `RelationTypeNotInSchema` (0x004C) | Relation type qname is not declared in the active schema for this namespace. Returned in strict mode only. |

#### 3.5 Not found (Category: `NotFound`)

| Code | Meaning |
|---|---|
| `MemoryNotFound` | MemoryId references a memory that doesn't exist or is forgotten/reclaimed |
| `ContextNotFound` | ContextId not in the agent's namespace |
| `SubscriptionNotFound` | Stream isn't an active subscription |
| `SnapshotNotFound` | Snapshot name doesn't exist |
| `TxnNotFound` | Transaction id not active |

#### 3.6 Conflict (Category: `Conflict`)

| Code | Meaning |
|---|---|
| `IdempotencyConflict` | Same request_id with different parameters |
| `TransactionConflict` | Transaction commit failed due to a conflict (e.g., a referenced memory was forgotten between operations) |
| `TransactionTimeout` | Transaction timed out before commit |
| `StreamIdInUse` | Tried to open a stream with an already-active stream_id |
| `SubscriptionLsnTooOld` | Subscription resumption LSN is past WAL retention |
| `CardinalityViolation` (0x0065) | RELATION_CREATE would violate the declared cardinality (`one_to_one` / `many_to_one` / `one_to_many`). The existing edge must be explicitly `RELATION_SUPERSEDE`d first. |

#### 3.7 Resource exhausted (Category: `ResourceExhausted`)

| Code | Meaning |
|---|---|
| `OutOfSlots` | Arena has no free slots |
| `OutOfDisk` | Disk full |
| `OutOfMemory` | Process memory exhausted |
| `RateLimited` | Per-connection or per-agent rate limit exceeded |
| `StreamLimitExceeded` | Per-connection concurrent stream limit |
| `ConnectionLimitExceeded` | Per-agent or per-IP connection limit |
| `TransactionLimitExceeded` | Per-agent active-transaction limit |

#### 3.8 Internal (Category: `Internal`)

| Code | Meaning |
|---|---|
| `Internal` (0x0080) | Generic internal error (server bug) |
| `StorageError` (0x0081) | Storage layer failed |
| `IndexError` (0x0082) | Vector index failed |
| `EmbeddingError` (0x0083) | Embedding layer failed |
| `MetadataError` (0x0084) | Metadata store failed |
| `Cancelled` (0x0085) | The operation was cancelled (client `CANCEL_STREAM`, server shutdown, or a parent stream aborted). Carries no `retry_after_ms`; clients MAY retry. |

#### 3.9 Unavailable (Category: `Unavailable`)

| Code | Meaning |
|---|---|
| `ShardUnavailable` (0x0090) | Shard not currently servable (e.g., during rebalance) |
| `Overloaded` (0x0091) | Server temporarily overloaded |
| `Restarting` (0x0092) | Server is restarting (drain mode) |
| `Maintenance` (0x0093) | Server is in maintenance mode |
| `RetrieverDegraded` (0x0094) | Reserved for admin and diagnostic surfaces (`/health`, `ADMIN_STATUS`) when a shard reports a degraded retriever set — e.g. a tantivy segment corruption or a graph-store `pwritev2` failure observed after spawn. Never returned to a normal RECALL: shards refuse to spawn if a required retriever is unwired, so a wired retriever failing at query time propagates as an internal error rather than a downgrade signal. There is no client-visible recovery action; the remedy is operator intervention. |

#### 3.10 Typed-graph errors

Typed-graph opcodes (the `0x01xx` namespace) surface their own error codes. Codes share the same namespace discipline as opcodes — substrate codes occupy `0x00xx`, typed-graph codes occupy `0x01xx`, with the low-byte family mirroring the typed-graph opcode ranges (schema at `0x012x`, entity at `0x013x`, statement at `0x014x`, query at `0x016x`, extractor at `0x017x`). All codes ride the same ERROR frame and are mapped into substrate categories for retry behavior.

| Code | Name | Family | Category |
|---|---|---|---|
| `0x0120` | `SchemaInvalid` | Schema | Validation |
| `0x0121` | `SchemaMigrationRequired` | Schema | Conflict |
| `0x0130` | `EntityNotFound` | Entity | NotFound |
| `0x0131` | `EntityTypeMismatch` | Entity | Validation |
| `0x0132` | `EntityAmbiguous` | Entity | Conflict |
| `0x0133` | `EntityMergeConflict` | Entity | Conflict |
| `0x0140` | `StatementNotFound` | Statement | NotFound |
| `0x0141` | `StatementObjectTypeMismatch` | Statement | Validation |
| `0x0142` | `StatementContradictsExisting` | Statement | Conflict |
| `0x0160` | `QueryTimeout` | Query | Unavailable |
| `0x0161` | `QueryOverBudget` | Query | ResourceExhausted |
| `0x0170` | `ExtractorDisabled` | Extractor | Conflict |
| `0x0171` | `ExtractorBudgetExceeded` | Extractor | ResourceExhausted |
| `0x0172` | `ExtractionFailed` | Extractor | Internal |

The namespace byte (`0x01`) tells operators reading a code at a glance that the failure originated in a typed-graph operation; the low-byte family (`0x4x`, `0x6x`, etc.) identifies which record family raised it. `ErrorCodeWire` is a `u16` enum carrying both substrate (`0x00xx`) and typed-graph (`0x01xx`) variants under one type.

Cardinality violations on RELATION_CREATE surface as the substrate-wide `CardinalityViolation` (0x0065, §3.6) — there is no typed-graph–local code for it. Open-vocabulary qname rejections in strict mode surface as `PredicateNotInSchema` (0x004B) / `RelationTypeNotInSchema` (0x004C) under §3.4. Retriever degradation surfaces as `RetrieverDegraded` (0x0094) under §3.9.

##### 3.10.1 Retry consequences for typed-graph codes

- `QueryTimeout` (Unavailable) and `ExtractorBudgetExceeded` (ResourceExhausted) are retryable — clients should back off and retry, possibly with reduced top_k / depth.
- `EntityAmbiguous` and `EntityMergeConflict` (Conflict) are **not** retryable on their own; resolution is a human / admin action via `ADMIN_RESOLVE_AMBIGUITY`.
- `ExtractionFailed` (Internal) is retryable; the extractor cache (§22) keeps the same body / model fingerprint so a retry usually hits cached results.

##### 3.10.2 Schema-not-declared mode

When no schema has been declared for a namespace, typed-graph writes (`STATEMENT_CREATE`, `RELATION_CREATE`) and reads (`QUERY`, etc.) accept any predicate / relation-type qname — the registry interns it on first use with `SchemaOrigin::ImplicitFromWrite` / `RelationTypeOrigin::ImplicitFromWrite`. No `SchemaNotDeclared` error is returned for these opcodes.

`SchemaNotDeclared` remains reserved for explicit schema-introspection opcodes (e.g. `SCHEMA_GET` against a namespace that has never had one), where there is nothing to return. Its category is `Conflict`.

The substrate cognitive primitives (the `0x00xx` namespace) are unaffected and continue to work normally in both modes.

### 4. ErrorDetails

The optional `ErrorDetails` carries per-error context:

```rust
struct ErrorDetails {
    field: Option<String>,                   // which field was invalid
    expected: Option<String>,                // what was expected
    actual: Option<String>,                  // what was received
}
```

Used primarily for `Validation` category errors to indicate exactly which field of the request was the problem.

For `IdempotencyConflict`, `field` may be the parameter that differs; `expected` is the original value, `actual` is the new (conflicting) value.

For `Conflict` errors, `details` may indicate which entity caused the conflict.

#### 4.1 Conventions for typed-graph errors

Typed-graph handlers populate `ErrorDetails` per the following conventions:

| Code | `field` | `expected` | `actual` |
|---|---|---|---|
| `EntityTypeMismatch` | `"entity_type_id"` | list of valid ids | the supplied id |
| `EntityAmbiguous` | `"canonical_name"` | (empty) | newline-joined existing EntityIds |
| `EntityMergeConflict` | `"merge"` | reason (e.g. "grace period expired", "already merged") | (empty) |
| `StatementObjectTypeMismatch` | predicate name | expected object type | actual encountered |
| `CardinalityViolation` | `"cardinality"` | declared rule (e.g. "one_to_many") | (empty) |
| `QueryTimeout` | `"timeout_ms"` | wall budget | elapsed |
| `ExtractorBudgetExceeded` | extractor name | tier budget | usage |

Free-form `message` accompanies every error and is intended for log lines, not programmatic dispatch.

#### 4.2 Audit trail for typed-graph errors

Errors that originate from state-mutating typed-graph opcodes (CREATE / UPDATE / RENAME / MERGE / TOMBSTONE / SCHEMA_UPLOAD / extractor governance ops) are written to the `entity_resolution_audit` / `schema_audit` tables (see [`../10_metadata/03_substrate_tables.md`](../10_metadata/03_substrate_tables.md) §"Audit") regardless of whether the operation succeeded. The audit row's outcome is the relevant error code.

Read-only errors (`ENTITY_GET` returning `EntityNotFound`, etc.) are not audited.

### 5. retry_after_ms

For retryable errors (`ResourceExhausted`, `Unavailable`), the server may include `retry_after_ms` — a suggested delay before the client retries.

The client SHOULD honor this hint. Ignoring it (retrying immediately) likely produces another retry response, congesting the server.

For `RateLimited`, `retry_after_ms` is typically the time until the rate-limit window resets.
For `Overloaded`, it's the server's estimate of when load might subside.

### 6. Client retry guidance

The recommended client retry policy:

| Error category | Action |
|---|---|
| `Protocol` | Don't retry; report as a bug |
| `Authentication` | Refresh credentials; retry once |
| `Authorization` | Don't retry |
| `Validation` | Don't retry; fix the request |
| `NotFound` | Don't retry |
| `Conflict` | Don't retry (semantic) |
| `ResourceExhausted` | Retry after `retry_after_ms` (or default backoff) |
| `Internal` | Retry once with exponential backoff; report if persistent |
| `Unavailable` | Retry after `retry_after_ms` with backoff |

For idempotent operations (those with `request_id`), retries are safe — duplicates are deduplicated. For non-idempotent operations (`PLAN`, `REASON`), retries may produce different but valid results.

### 7. Error propagation

#### 7.1 Per-stream

Most errors are per-stream: the error frame goes on the stream that errored. Other streams on the same connection continue normally.

#### 7.2 Connection-level

Some errors are connection-level: wire-version mismatch, AUTH failure, malformed frames at any time. These come on `stream_id = 0` and are followed by connection close.

#### 7.3 During streaming responses

If an error occurs mid-stream (e.g., a `RECALL` partially completes, then hits a storage error):

- The server sends an `ERROR` frame on the stream.
- The error frame implicitly carries EOS.
- The client treats prior frames as valid; the error indicates incomplete completion.

### 8. Wire format example: validation error

A client sends `ENCODE_REQ` with empty text. The server responds:

```
S → C: ERROR(stream_id=<encode's stream>, EOS)
       payload:
         code: TextEmpty
         category: Validation
         message: "Text field cannot be empty"
         details:
           field: Some("text")
           expected: Some("non-empty UTF-8 string")
           actual: Some("")
         retry_after_ms: None
```

A client maps this to a typed error (`InvalidArgumentError`) and surfaces it.

### 9. Wire format example: stream cancellation acknowledged

Strictly, this isn't an error — `CANCEL_STREAM_ACK` is a normal response. But the cancelled stream's terminal frame may carry an explicit cancellation indication:

```
S → C: ERROR(stream_id=<cancelled stream>, EOS)
       payload:
         code: Cancelled (0x0085)
         category: Internal
         message: "Stream cancelled by client"
```

`Cancelled` is the assigned `Internal`-category code (§3.8). It is reused for client `CANCEL_STREAM`, server shutdown mid-stream, and parent-stream aborts. The category is `Internal` rather than a new `Cancelled` category because the retry policy is identical to other `Internal` codes (clients MAY retry; no specific `retry_after_ms` is meaningful).

### 10. Limits on error verbosity

Error messages SHOULD be human-readable but limited:

- `message` field: max 1024 bytes.
- `ErrorDetails.field/expected/actual`: max 512 bytes each.

Error messages do not include sensitive information (e.g., token contents, raw query data). Operators may further configure the verbosity (production servers may emit shorter messages than dev servers).

### 11. Localization

Error messages are in English. The protocol does not currently support localization.

If a future major version adds localization, the message would carry a structured key (`ErrorCode` already provides this) and the human-readable string would be language-dependent. Currently Brain uses English only.

### 12. Error logging

The server logs errors at appropriate levels:

- `Validation`, `NotFound`, `Conflict` — INFO (these are normal client-side issues, not server problems).
- `Authentication`, `Authorization` — WARN (security-relevant).
- `Protocol` — WARN (likely client bug).
- `ResourceExhausted` — WARN.
- `Internal`, `Unavailable` — ERROR (server-side problem).

Each log entry includes the connection's session_id and the stream_id of the affected stream, for correlation.

## Validation

What the server validates when it receives a frame, and what it rejects. The protocol assumes adversarial input and validates aggressively.

The validation rules MUST be implemented by every conforming server. Clients SHOULD perform the same validation locally to fail fast on bugs without round-tripping to the server.

### 13. Layered validation

Validation happens in three layers, in order:

1. **Frame-level** — the 32-byte header is parsed and structurally validated. Bad framing closes the connection.
2. **Payload-level** — the payload is decoded (CBOR for structured fields, little-endian f32 for vectors) and structurally validated. Bad payloads return an error frame.
3. **Operation-level** — the parameters of a specific opcode are validated against the data model. Bad parameters return an opcode-specific error frame.

Earlier failures take precedence over later ones. If the frame's header is malformed, the payload is never decoded.

### 14. Frame-level validation

For every incoming frame, the server checks:

#### 14.1 Magic bytes

The first four bytes MUST be `BRN0` (0x42, 0x52, 0x4E, 0x30). Any deviation closes the connection without sending an error — the peer is not speaking Brain.

**Rationale.** Magic-byte mismatch usually means the connection got garbled (TLS misconfiguration, port confusion). Returning an error frame would imply that the server is willing to talk Brain; closing silently is the safer response to "this is not Brain traffic".

#### 14.2 Version field

The version field MUST be a wire-protocol version the server supports (currently 1). If the version is unknown:

- During handshake (before WELCOME): the server returns `WireVersionNegotiationFailed` and closes.
- After handshake: the server returns `BadFrame` and closes — receiving an unexpected version after handshake means client/server got out of sync.

#### 14.3 Header CRC32C

The header carries a CRC32C of bytes [0..28]. The server recomputes and compares. Mismatch closes the connection — header corruption typically means deeper transport corruption that further communication cannot recover from.

#### 14.4 Payload length bounds

The 24-bit `payload_len` MUST be ≤ 16 MiB (2^24 - 1). Larger values close the connection.

The server's *effective* limit is configurable, defaulting to 16 MiB. A frame with `payload_len > effective_limit` returns `PayloadTooLarge` and closes the stream (but not the connection, unless many such frames arrive).

#### 14.5 Stream ID rules

Stream IDs MUST follow the parity convention from [`06_streaming.md`](06_streaming.md):

- Client-initiated streams: odd values.
- Reserved for server-initiated: even values (not currently used).
- Stream ID 0: reserved for connection-level frames (HELLO, WELCOME, PING, PONG, BYE).

Violations return `BadFrame`.

#### 14.6 Opcode validity

The opcode MUST be a known value. Unknown opcodes return `UnknownOpcode`. The connection stays open; the client can send other valid opcodes.

The set of valid opcodes is defined in [`03_opcodes.md`](03_opcodes.md). Adding a new opcode is a wire-version bump.

### 15. Payload-level validation

After the frame header is accepted, the payload is decoded.

#### 15.1 Payload CRC32C

The payload's CRC32C in the header is recomputed against the actual payload bytes. Mismatch returns `PayloadCorrupted`.

For zero-length payloads, the payload CRC field is 0; no CRC check.

#### 15.2 CBOR structural validation

For structured payloads encoded with CBOR:

- The payload MUST decode without error using a standards-conformant CBOR decoder. This catches:
  - Truncated payloads.
  - Out-of-bounds offsets within the payload.
  - Type tag mismatches.
- Decoded values MUST satisfy any per-field constraints documented in the opcode's request shape (see [`05_frame_layouts.md`](05_frame_layouts.md)).

Failure returns `BadPayload`.

#### 15.3 bytemuck raw payloads

For raw byte payloads (vectors via `ENCODE_VECTOR_DIRECT`):

- The payload length MUST exactly equal `dim * sizeof(f32)` (1536 bytes for the 384-dim vector).
- Length mismatch returns `BadPayload`.

After length check, the bytes are interpreted as `[f32; 384]`. Element-level validation (NaN, Inf, normalization) is a separate operation-level check.

### 16. Operation-level validation

Per-opcode validation, after the payload has decoded successfully.

#### 16.1 ENCODE

- `text` MUST be valid UTF-8.
- `text` MUST NOT be empty after Unicode whitespace trim.
- `text.len()` MUST be ≤ `max_text_bytes` (default 1 MiB, configurable).
- `request_id` MUST NOT be the all-zero `RequestId`.
- `salience_hint`, if present, MUST be in [-1.0, +1.0]. NaN/Inf rejected.
- `context_id` MUST exist for the agent's namespace (or the operation specifies a context name to lazy-create).
- `kind`, if explicitly set, MUST be a valid `MemoryKind`. Server may reject `kind = Consolidated` since clients are forbidden from creating consolidated memories.
- Outgoing edges in the same payload MUST reference existing memories of the same agent. Cross-agent or non-existent targets return `InvalidArgument`.

#### 16.2 ENCODE_VECTOR_DIRECT

All ENCODE rules plus:

- The vector MUST contain no NaN or Inf elements. Failing element returns `InvalidVector`.
- The vector's L2 norm MUST be in `[1.0 - 1e-3, 1.0 + 1e-3]`. Out-of-range returns `InvalidVector`.
- The supplied `embedding_model_fp` MUST match a model the server knows about. Unknown fingerprint returns `UnknownModel`.

#### 16.3 RECALL

- `cue_text` validation matches `ENCODE`'s `text`.
- `top_k` MUST be in `[1, 1000]`. Higher values capped to 1000.
- `confidence_min`, if present, MUST be in [0.0, 1.0]. NaN/Inf rejected.
- `age_bound`, if present, MUST be a non-negative duration.
- `context_filter`, if present, MUST reference contexts that exist for the agent. Unknown contexts return `InvalidContext`.
- `kind_filter`, if present, MUST contain only valid `MemoryKind` values.

#### 16.4 PLAN, REASON

- `start_state` and `goal_state` (for PLAN) MUST be either valid `MemoryId`s or non-empty text.
- `MemoryId` references MUST belong to the agent.
- `budget` MUST have at least one dimension specified (max steps, max wall time, or max branches). At least one bound is required to prevent unbounded search.

#### 16.5 FORGET

- `memory_id` MUST be a valid `MemoryId` belonging to the agent.
- `request_id` MUST NOT be the all-zero value.
- `mode` MUST be either `Soft` or `Hard`.

#### 16.6 SUBSCRIBE

- `filter` parameters validated per their types.
- `from_lsn`, if present, MUST be ≤ the server's current LSN. Future LSNs return `InvalidArgument`.
- `from_lsn` older than the WAL retention horizon returns `LsnTooOld`.

#### 16.7 TXN_*

- `txn_id` MUST be a non-zero `TxnId`.
- `TXN_BEGIN` for an already-active `txn_id` returns `TransactionExists`.
- `TXN_COMMIT` and `TXN_ABORT` for unknown `txn_id` return `UnknownTransaction`.
- Operations within a transaction MUST carry the matching `txn_id`. Mismatched ids return `WrongTransaction`.

#### 16.8 ADMIN_*

- The session MUST have admin privileges (set during AUTH). Otherwise `Unauthorized`.
- Per-admin-opcode parameter validation as documented in the opcode's request shape.

#### 16.9 Typed-graph opcodes (`0x01xx`)

Typed-graph opcodes use the same two-layer discipline: CBOR structural validation at the wire layer, then handler-layer semantic validation. The rules below run before any storage call.

##### 16.9.1 Universal field caps

Applied to every string / blob field across the `0x01xx` namespace. Limits balance correctness (catch obvious bugs) against permissiveness (don't reject legitimate Unicode-heavy text).

| Field shape | Max size |
|---|---|
| Identifier-ish string (e.g. `canonical_name`, `predicate_name`, `schema_version`) | 256 bytes UTF-8 |
| Free-form text (e.g. `reason`, `context`, `message`) | 4096 bytes UTF-8 |
| Opaque blob (e.g. `attributes_blob`, `evidence_blob`) | 64 KiB |
| Collection (e.g. `aliases`, `candidate_ids`) | 256 elements |
| Cursor / pagination token | 1 KiB |

Violations return `InvalidArgument` (category `Validation`) with `details.field` naming the offender and `details.expected` carrying the limit.

##### 16.9.2 Entity opcodes (`0x0130–0x0138`)

**`ENTITY_CREATE` (0x0130).**

| Field | Rule | Error code |
|---|---|---|
| `entity_type_id` | must be > 0 and registered in `entity_types` table | `EntityTypeMismatch` |
| `canonical_name` | non-empty after `.trim()`; ≤ 256 bytes; valid UTF-8 (validated on decode) | `InvalidArgument` |
| `canonical_name` (after server-side `normalize_name`) | must not collide with an existing entity of the same `entity_type_id` | `EntityAmbiguous` (duplicate) |
| `aliases.len()` | ≤ 32 | `InvalidArgument` |
| each `alias` | non-empty after `.trim()`; ≤ 256 bytes | `InvalidArgument` |
| `attributes_blob` | ≤ 64 KiB | `InvalidArgument` |
| `request_id` | non-zero UUIDv7 | `InvalidArgument` |

Aliases are deduplicated server-side on the normalized form before insertion. A request supplying duplicates is **not** rejected — duplicates are silently collapsed.

**`ENTITY_GET` (0x0131).** `entity_id` must be a non-zero UUIDv7 (else `InvalidArgument`). No further validation — missing rows return `EntityNotFound`.

**`ENTITY_UPDATE` (0x0132).** Same caps as `ENTITY_CREATE`, plus:

- `entity_id` must be non-zero and exist (else `EntityNotFound`).
- If `canonical_name` differs from the current row's normalized form, treat as an implicit rename and apply rename rules.
- `entity_type_id` is ignored (`ENTITY_UPDATE` cannot retype). A future opcode handles retypes; the wire field is reserved for that use.

**`ENTITY_RENAME` (0x0133).**

| Field | Rule | Error code |
|---|---|---|
| `entity_id` | non-zero; entity must exist; must not be tombstoned | `EntityNotFound` |
| `new_canonical_name` | non-empty; ≤ 256 bytes | `InvalidArgument` |
| `new_canonical_name` (normalized) | must not collide with an existing entity of the same `entity_type_id` | `EntityAmbiguous` |
| `move_to_alias` | currently must be `true` | `InvalidArgument` |

**`ENTITY_MERGE` (0x0134).**

| Field | Rule | Error code |
|---|---|---|
| `survivor`, `merged` | both non-zero; both exist; both same `entity_type_id` (cross-type merge currently forbidden) | `EntityNotFound` / `EntityTypeMismatch` |
| `survivor == merged` | rejected | `EntityMergeConflict` |
| `merged.merged_into` | must be `None` (no double-merge) | `EntityMergeConflict` |
| `survivor.merged_into` | must be `None` | `EntityMergeConflict` |
| `confidence` | in `[0.0, 1.0]`; finite | `InvalidArgument` |
| `confidence` ≥ 0.7 | otherwise rejected (merge candidates require ≥ 0.7) | `InvalidArgument` |
| `reason` | ≤ 4096 bytes | `InvalidArgument` |

**`ENTITY_UNMERGE` (0x0135).**

| Field | Rule | Error code |
|---|---|---|
| `merged_entity` | non-zero; must exist | `EntityNotFound` |
| `merged_entity.merged_into` | must be `Some(_)` | `EntityNotFound` |
| merge audit `created_at + grace_period` | must be > now | `EntityMergeConflict` |

**`ENTITY_RESOLVE` (0x0136).**

| Field | Rule | Error code |
|---|---|---|
| `candidate_name` | non-empty after `.trim()`; ≤ 256 bytes | `InvalidArgument` |
| `context` | ≤ 4096 bytes; handler truncates to first 100 chars before passing to resolver | `InvalidArgument` |
| `entity_type_hint` | `0` allowed (no hint); otherwise must be registered | `EntityTypeMismatch` |
| schema declared? | required | `SchemaNotDeclared` |

**`ENTITY_LIST` (0x0137).**

| Field | Rule | Error code |
|---|---|---|
| `entity_type_id` | `0` (no filter) or registered | `EntityTypeMismatch` |
| `name_prefix` | ≤ 256 bytes; server normalizes before prefix-matching | `InvalidArgument` |
| `limit` | 1 ≤ limit ≤ 1000 | `InvalidArgument` |
| `cursor` | ≤ 1 KiB; server-defined opaque shape; malformed → reject | `InvalidArgument` |

**`ENTITY_TOMBSTONE` (0x0138).**

| Field | Rule | Error code |
|---|---|---|
| `entity_id` | non-zero; must exist; already-tombstoned returns success (idempotent) | `EntityNotFound` |
| `reason` | ≤ 4096 bytes | `InvalidArgument` |

##### 16.9.3 Schema opcodes (`0x0120–0x0126`)

- `schema_document` (`SCHEMA_UPLOAD` / `SCHEMA_VALIDATE`): ≤ 1 MiB (raised from the universal 64 KiB cap; schema documents are intentionally larger).
- `version_id` (`SCHEMA_GET`): `0` means "latest"; otherwise must exist → `SchemaInvalid`.
- `extractor_id`: must be in the active extractor registry → `InvalidArgument` otherwise.

##### 16.9.4 Statement opcodes (`0x0140–0x0146`)

- `subject`, `object` (when `EntityRef`): must resolve to existing entity → `EntityNotFound`.
- `predicate`: a `"namespace:name"` qname. Open-vocabulary in schemaless mode (interned on first use with `SchemaOrigin::ImplicitFromWrite`); strict mode rejects unknown qnames with `PredicateNotInSchema` (0x004B). Declared object-type constraints → `StatementObjectTypeMismatch`.
- `evidence_blob`: ≤ 64 KiB.
- `confidence`: in `[0.0, 1.0]`.

##### 16.9.5 Relation opcodes (`0x0150–0x0156`)

- `relation_type`: a `"namespace:name"` qname. Open-vocabulary in schemaless mode (interned on first use with `RelationTypeOrigin::ImplicitFromWrite`, default `cardinality: many_to_many`); strict mode rejects unknown qnames with `RelationTypeNotInSchema` (0x004C).
- `from`, `to`: must be existing entities; for schema-declared types, endpoint entity types must match the relation's declared signature → `EntityTypeMismatch`. Implicit types skip this check.
- cardinality (`one_to_one` / `one_to_many` / etc.): enforced server-side on schema-declared types only → `CardinalityViolation` (0x0065).

##### 16.9.6 Query opcodes (`0x0160–0x0163`)

- `top_k`: 1 ≤ top_k ≤ 1000.
- `depth` (for `RELATION_TRAVERSE`-shaped queries): 1 ≤ depth ≤ 8.
- `budget_wall_time_ms`: 1 ≤ budget ≤ 60000 (60 s ceiling).
- empty filter clauses are allowed (no-op); empty `text` for `RECALL_HYBRID` rejected.

##### 16.9.7 Admin opcodes (`0x0170–0x0177`)

- `audit_id`, `job_id`: non-zero UUIDv7; existence checked at handler.
- `extractor_ids` (for `ADMIN_BACKFILL`): non-empty; all must be registered.
- `memory_range`: `start ≤ end` (unix nanos) → `InvalidArgument`.

##### 16.9.8 Constants

Centralized so clients and the server agree on the same numbers. Defined at `brain-core::knowledge::validation`:

```rust
pub const MAX_IDENT_BYTES: usize        = 256;
pub const MAX_FREEFORM_BYTES: usize     = 4096;
pub const MAX_BLOB_BYTES: usize         = 64 * 1024;
pub const MAX_COLLECTION_ELEMENTS: usize = 256;
pub const MAX_CURSOR_BYTES: usize       = 1024;
pub const MAX_ALIASES_PER_ENTITY: usize = 32;
pub const MAX_SCHEMA_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_TOP_K: u32                = 1000;
pub const MAX_TRAVERSE_DEPTH: u32       = 8;
pub const MAX_QUERY_WALL_TIME_MS: u32   = 60_000;
pub const MIN_MERGE_CONFIDENCE: f32     = 0.7;
```

##### 16.9.9 Validation order for typed-graph requests

Per typed-graph request, the server runs validations in this order. The first failure short-circuits with the corresponding error:

1. **CBOR structural check** (decode + schema). `MalformedPayload` → close frame, keep connection.
2. **Universal field caps** (§16.9.1). `InvalidArgument`.
3. **Op-specific field-level rules** (§16.9.2 – §16.9.7). Various codes per table.
4. **Cross-field rules** (e.g. `survivor != merged`). Op-specific codes.
5. **Existence / registry checks** (entity exists, type registered, schema declared). Op-specific codes.
6. **Idempotency replay check**. Cached response returned if `request_id` is a duplicate match; mismatch raises `IdempotencyConflict`.
7. **Handler proceeds.** Errors from this point are storage / commit failures (`Internal`).

### 17. Cross-frame validation

Some validations span multiple frames.

#### 17.1 Stream lifecycle

Frames within a stream MUST follow the lifecycle rules from [`06_streaming.md`](06_streaming.md):

- A stream is opened by the first request frame.
- Subsequent frames on the stream MUST be either continuation or response frames belonging to that opcode's pattern.
- A stream is closed by the end-of-stream frame.
- Frames on a closed stream are rejected with `BadStream`.

#### 17.2 Handshake order

Operations are rejected with `NotHandshaked` until WELCOME has been received.
Operations requiring authentication are rejected with `NotAuthenticated` until AUTH_OK has been received.

#### 17.3 Idempotency consistency

If the same `RequestId` is reused with different parameters (different `text`, different `context_id`, etc.), the server returns `IdempotencyConflict`.

If the same `RequestId` is reused with identical parameters, the server replays the original response.

#### 17.4 Transactional ordering

Operations within a transaction are validated as if the transaction's prior operations have been applied. For example, an `ENCODE` followed by a `LINK` referencing the encoded memory's id is validated only at commit; until then, the link is buffered.

If validation fails at commit, the entire transaction is aborted with `TransactionValidationFailed` and a description of which operation failed and why.

### 18. Rate and resource limits

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

### 19. Validation determinism

Validation MUST be deterministic for a given input. The same payload, against the same configuration, MUST always produce the same accept/reject decision.

This matters for testing and debugging: a frame that fails validation in production should fail validation in a developer's reproduction. Non-deterministic validation (e.g., based on memory pressure or time) is forbidden — resource-pressure responses are handled at a different layer.

### 20. The "fail closed" principle

When a validation rule is unclear or under-specified, the server MUST reject. New, unspecified fields, novel opcode parameters, ambiguous flag combinations: all reject by default.

This is the opposite of "best effort". The protocol's premise is that the client provides correct frames; anything unrecognized is treated as a bug or attack, not as a feature to be inferred.

### 21. Validation error reporting

Validation errors return error frames containing:

- The error code (one of the values in §3 above).
- A human-readable message describing what failed.
- The stream ID of the failing frame.
- For payload validation, the offset within the payload where validation failed (best-effort; not always available).

The server SHOULD include enough detail to diagnose the issue without leaking internal state. For example: "field `top_k` out of range; got 5000, max 1000" is fine; "validation failed at internal token 0xDEADBEEF" is not.

### 22. The validation budget

Validation is on the hot path. Each validation step adds latency. The budget for validation cost is:

- Frame-level: < 100 ns per frame (CRC plus a few comparisons).
- Payload-level CBOR decode: single-digit µs typical, larger for bigger payloads.
- Payload-level bytemuck cast: < 100 ns (no copying, just a length check and a pointer reinterpret).
- Operation-level: < 5 µs typical.

Validators MUST stay within these budgets. Heavyweight checks (e.g., re-embedding to verify a vector) belong in operation handlers, not validation.

---

*Continue to [`08_typed_graph_frames.md`](08_typed_graph_frames.md) for typed-graph noun frames.*
