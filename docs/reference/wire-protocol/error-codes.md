# Wire error codes

Brain returns errors as `Error(0x00FF)` frames bound to a
`stream_id`. The payload carries an `ErrorCode` variant plus a
human-readable message and optional details.

**Source:** `crates/brain-protocol/src/error.rs`.
**Spec:** §03/10.

## Categories

Errors group into nine categories. Each maps to a retryability
class (`error.rs:52–57`):

| Category | Retryable? | Meaning |
|---|---|---|
| `Protocol` | no | The frame itself is malformed. Don't retry; fix the client. |
| `Authentication` | no | AUTH was rejected, missing, or expired. |
| `Authorization` | no | The agent lacks permission for this op. |
| `Validation` | no | Request fields didn't pass validation. |
| `NotFound` | no | The targeted resource doesn't exist. |
| `Conflict` | no | Idempotency or transaction conflict. |
| `ResourceExhausted` | **yes** | Slot / disk / memory / rate cap hit; back off and retry. |
| `Internal` | **yes** | Server bug or transient infrastructure failure. |
| `Unavailable` | **yes** | Shard restarting, overloaded, or in maintenance. |

Idempotent operations (those carrying `request_id`) are always
safe to retry on a retryable category; same `request_id` will
hit the dedupe cache and return the original response.

---

## Catalog

### Protocol

| Code | Returned when |
|---|---|
| `BadMagic` | First 4 bytes of the frame aren't `BRN0`. Usually a non-Brain client connected to `listen_addr`. |
| `BadHeaderCrc` | The header CRC32C didn't match. Network corruption or bug in client. |
| `BadPayloadCrc` | Payload CRC didn't match. As above. |
| `BadOpcode` | Opcode is unknown, or used in the wrong direction. |
| `BadVersion` | Frame's `version` byte doesn't match the negotiated version. |
| `BadFrame` | Generic malformed frame (catch-all). |
| `OversizePayload` | `payload_len` exceeded `MAX_PAYLOAD_BYTES` (16 MiB − 1). |
| `ReservedFieldNonZero` | `reserved_a` or `reserved_b` wasn't zero. |
| `BadFlagCombination` | Frame flags violate a mutual exclusion (e.g. `MPL` + `EOS` on a single-frame body). |
| `MalformedRkyv` | The `rkyv` portion of the payload didn't pass `check_archived_root`. |
| `MalformedVector` | Raw vector bytes don't match the declared dim, or fail the norm check. |
| `VersionNotSupported` | No mutual version between client and server in HELLO/WELCOME negotiation. |
| `NoSuchAuthMethod` | AUTH's `method` not in WELCOME's `auth_methods`. |

### Authentication

| Code | Returned when |
|---|---|
| `Unauthenticated` | AUTH credentials rejected by the auth backend. |
| `NotAuthenticated` | Op attempted before AUTH_OK. |
| `AuthBackendUnavailable` | Auth backend (e.g. JWT verifier) unreachable. |
| `SessionExpired` | Session id timed out per `idle_timeout_seconds`. |

### Authorization

| Code | Returned when |
|---|---|
| `PermissionDenied` | Agent lacks the relevant `AgentPermissions` bit. |
| `AdminPermissionRequired` | Op requires `can_admin`, agent doesn't have it. |
| `WrongShard` | Request targets a shard the connection isn't bound to. |

### Validation

| Code | Returned when |
|---|---|
| `InvalidArgument` | A request field failed validation (generic). |
| `MissingRequiredField` | A required field was absent. |
| `TextTooLarge` | `text` exceeded the per-encode cap (~1 MB). |
| `TextEmpty` | `text` was empty. |
| `BadContextId` | `context` not valid for this agent. |
| `BadMemoryKind` | `kind` not one of `Episodic` / `Semantic` / `Consolidated` (or `Consolidated` was client-set, which is worker-only). |
| `BadEdgeKind` | Edge kind not one of the eight enumerated. |
| `BadStrategyHint` | Recall strategy hint isn't recognised. |
| `TopKOutOfRange` | `k` < 1 or > 1 000. |
| `BudgetTooLarge` | PLAN / REASON budget exceeded the per-op cap. |
| `BadModelFingerprint` | Embedding-model fingerprint unknown. |

### NotFound

| Code | Returned when |
|---|---|
| `MemoryNotFound` | `MemoryId` doesn't exist (or was forgotten + reclaimed). |
| `ContextNotFound` | `ContextId` not in agent's namespace. |
| `SubscriptionNotFound` | `stream_id` not an active subscription. |
| `SnapshotNotFound` | Snapshot id doesn't exist. |
| `TxnNotFound` | Transaction id not active. |

### Conflict

| Code | Returned when |
|---|---|
| `IdempotencyConflict` | Same `request_id` reused with different parameters. |
| `TransactionConflict` | Optimistic-conflict on commit. |
| `TransactionTimeout` | Transaction exceeded `max_wall_time_sec`. |
| `StreamIdInUse` | Client opened a stream with an already-active `stream_id`. |
| `SubscriptionLsnTooOld` | Subscription resume LSN is past the WAL-retention window. |

### ResourceExhausted *(retryable)*

| Code | Returned when |
|---|---|
| `OutOfSlots` | Arena has no free slot. The slot-reclamation worker will eventually free some. |
| `OutOfDisk` | Disk full. See [`../../runbooks/disk-filling.md`](../../runbooks/disk-filling.md). |
| `OutOfMemory` | Process RSS hit OS limit. See [`../../runbooks/memory-pressure.md`](../../runbooks/memory-pressure.md). |
| `RateLimited` | Per-connection or per-agent rate cap hit. |
| `StreamLimitExceeded` | Per-connection `max_concurrent_streams` reached. |
| `ConnectionLimitExceeded` | Per-agent or per-IP connection cap. |
| `TransactionLimitExceeded` | Per-agent active-txn cap (default 16). |

### Internal *(retryable)*

| Code | Returned when |
|---|---|
| `Internal` | Generic server bug. Always reported via tracing. |
| `StorageError` | Arena / WAL layer failed. |
| `IndexError` | HNSW layer failed. |
| `EmbeddingError` | Embedder failed (model not loaded, GPU error). |
| `MetadataError` | redb layer failed. |

### Unavailable *(retryable)*

| Code | Returned when |
|---|---|
| `ShardUnavailable` | Shard restarting or in a forced-drain state. |
| `Overloaded` | Server's adaptive load shed kicked in. |
| `Restarting` | Server is draining for a planned restart. |
| `Maintenance` | Operator put the server in maintenance mode. |

---

## HTTP-surface mapping

The admin HTTP endpoints reuse these codes (see
[`../http-api.md`](../http-api.md)):

| Category | HTTP status |
|---|---|
| `Protocol`, `Validation` | 400 |
| `Authentication` | 401 |
| `Authorization` | 403 |
| `NotFound` | 404 |
| `Conflict` | 409 |
| `ResourceExhausted` | 429 |
| `Unavailable` | 503 |
| `Internal` | 500 |

## See also

- [`frame-format.md`](frame-format.md) — where the `Error(0x00FF)` frame lives in the protocol.
- [`../../runbooks/`](../../runbooks/) — per-symptom incident response.

**Spec:** §03/10. **Source:** `crates/brain-protocol/src/error.rs`.
