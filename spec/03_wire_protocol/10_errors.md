# 03.10 Errors

This file specifies the protocol's error handling: error codes, categories, frame layouts, and propagation rules.

## 1. The ERROR frame

Every error is delivered as an `ERROR` frame (opcode `0xFF`). The error frame layout is in [`08_response_frames.md`](08_response_frames.md) §25.

An error frame has the same `stream_id` as the operation that errored — except for connection-level errors (during handshake, on stream_id 0), which use stream_id 0.

An error frame implicitly carries EOS for the stream it terminates. After receiving an error, the client SHOULD NOT expect any further frames on that stream.

## 2. Error categories

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

The category drives client retry behavior. The SDK uses category to decide whether to retry, propagate, or fail fast.

## 3. Error codes

A more specific error code accompanies each category. The complete table:

### 3.1 Protocol errors (Category: `Protocol`)

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
| `MalformedRkyv` | Rkyv-encoded payload didn't validate |
| `MalformedVector` | Raw vector bytes don't match declared dim or fail norm check |

### 3.2 Connection / handshake (Category: `Protocol` or `Authentication`)

| Code | Meaning |
|---|---|
| `VersionNotSupported` | No mutual version between client and server |
| `NoSuchAuthMethod` | AUTH method not in WELCOME's auth_methods |
| `Unauthenticated` | AUTH credentials rejected |
| `NotAuthenticated` | Operation attempted before AUTH_OK |
| `AuthBackendUnavailable` | Auth backend (e.g., token service) unreachable |
| `SessionExpired` | Session timed out (rare; sessions are connection-lifetime) |

### 3.3 Authorization (Category: `Authorization`)

| Code | Meaning |
|---|---|
| `PermissionDenied` | Agent lacks permission for this operation |
| `AdminPermissionRequired` | Operation requires `can_admin` |
| `WrongShard` | Operation tried to address a different shard than the connection's |

### 3.4 Validation (Category: `Validation`)

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

### 3.5 Not found (Category: `NotFound`)

| Code | Meaning |
|---|---|
| `MemoryNotFound` | MemoryId references a memory that doesn't exist or is forgotten/reclaimed |
| `ContextNotFound` | ContextId not in the agent's namespace |
| `SubscriptionNotFound` | Stream isn't an active subscription |
| `SnapshotNotFound` | Snapshot name doesn't exist |
| `TxnNotFound` | Transaction id not active |

### 3.6 Conflict (Category: `Conflict`)

| Code | Meaning |
|---|---|
| `IdempotencyConflict` | Same request_id with different parameters |
| `TransactionConflict` | Transaction commit failed due to a conflict (e.g., a referenced memory was forgotten between operations) |
| `TransactionTimeout` | Transaction timed out before commit |
| `StreamIdInUse` | Tried to open a stream with an already-active stream_id |
| `SubscriptionLsnTooOld` | Subscription resumption LSN is past WAL retention |

### 3.7 Resource exhausted (Category: `ResourceExhausted`)

| Code | Meaning |
|---|---|
| `OutOfSlots` | Arena has no free slots |
| `OutOfDisk` | Disk full |
| `OutOfMemory` | Process memory exhausted |
| `RateLimited` | Per-connection or per-agent rate limit exceeded |
| `StreamLimitExceeded` | Per-connection concurrent stream limit |
| `ConnectionLimitExceeded` | Per-agent or per-IP connection limit |
| `TransactionLimitExceeded` | Per-agent active-transaction limit |

### 3.8 Internal (Category: `Internal`)

| Code | Meaning |
|---|---|
| `Internal` | Generic internal error (server bug) |
| `StorageError` | Storage layer failed |
| `IndexError` | ANN index failed |
| `EmbeddingError` | Embedding layer failed |
| `MetadataError` | Metadata store failed |

### 3.9 Unavailable (Category: `Unavailable`)

| Code | Meaning |
|---|---|
| `ShardUnavailable` | Shard not currently servable (e.g., during rebalance) |
| `Overloaded` | Server temporarily overloaded |
| `Restarting` | Server is restarting (drain mode) |
| `Maintenance` | Server is in maintenance mode |

## 4. ErrorDetails

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

## 5. retry_after_ms

For retryable errors (`ResourceExhausted`, `Unavailable`), the server may include `retry_after_ms` — a suggested delay before the client retries.

The client SHOULD honor this hint. Ignoring it (retrying immediately) likely produces another retry response, congesting the server.

For `RateLimited`, `retry_after_ms` is typically the time until the rate-limit window resets.
For `Overloaded`, it's the server's estimate of when load might subside.

## 6. Client retry guidance

The SDK's recommended retry policy:

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

For idempotent operations (those with `request_id`), retries are safe — the substrate deduplicates. For non-idempotent operations (`PLAN`, `REASON`), retries may produce different but valid results.

## 7. Error propagation

### 7.1 Per-stream

Most errors are per-stream: the error frame goes on the stream that errored. Other streams on the same connection continue normally.

### 7.2 Connection-level

Some errors are connection-level: bad version negotiation, AUTH failure, malformed frames at any time. These come on `stream_id = 0` and are followed by connection close.

### 7.3 During streaming responses

If an error occurs mid-stream (e.g., a `RECALL` partially completes, then hits a storage error):

- The server sends an `ERROR` frame on the stream.
- The error frame implicitly carries EOS.
- The client treats prior frames as valid; the error indicates incomplete completion.

## 8. Wire format example: validation error

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

The client's SDK maps this to a typed exception (`InvalidArgumentError`) and surfaces it.

## 9. Wire format example: stream cancellation acknowledged

Strictly, this isn't an error — `CANCEL_STREAM_ACK` is a normal response. But the cancelled stream's terminal frame may carry an explicit cancellation indication:

```
S → C: ERROR(stream_id=<cancelled stream>, EOS)
       payload:
         code: Cancelled
         category: ... (a special Cancelled category? or Internal? — TBD)
         message: "Stream cancelled by client"
```

This is one of the open design questions: should cancellation be an error or a normal frame? See [`13_open_questions.md`](13_open_questions.md) WP-OQ-2.

## 10. Limits on error verbosity

Error messages SHOULD be human-readable but limited:

- `message` field: max 1024 bytes.
- `ErrorDetails.field/expected/actual`: max 512 bytes each.

The substrate doesn't include sensitive information in error messages (e.g., token contents, raw query data). Operators may further configure the verbosity (production servers may emit shorter messages than dev servers).

## 11. Localization

Error messages are in English. The protocol does not currently support localization.

If a future version adds localization, the message would carry a structured key (`ErrorCode` already provides this) and the human-readable string would be language-dependent. v1 just uses English.

## 12. Error logging

The server logs errors at appropriate levels:

- `Validation`, `NotFound`, `Conflict` — INFO (these are normal client-side issues, not server problems).
- `Authentication`, `Authorization` — WARN (security-relevant).
- `Protocol` — WARN (likely client bug).
- `ResourceExhausted` — WARN.
- `Internal`, `Unavailable` — ERROR (server-side problem).

Each log entry includes the connection's session_id and the stream_id of the affected stream, for correlation.

---

*Continue to [`11_validation.md`](11_validation.md) for validation rules.*
