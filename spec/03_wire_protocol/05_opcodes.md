# 03.05 Opcodes

The opcode is a single byte in the frame header. Server-bound opcodes (client → server) occupy 0x00–0x7F; client-bound opcodes (server → client) occupy 0x80–0xFF.

## 1. The complete table

### 1.1 Connection management

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x01 | `HELLO` | C → S | Initial frame; client identity and supported versions |
| 0x81 | `WELCOME` | S → C | Reply to HELLO; server identity, negotiated version, session_id |
| 0x02 | `AUTH` | C → S | Authentication credentials |
| 0x82 | `AUTH_OK` | S → C | Authentication success; bind to agent_id |
| 0x10 | `PING` | C → S | Keepalive |
| 0x90 | `PONG` | S → C | Response to PING |
| 0x91 | `SERVER_PING` | S → C | Server-initiated keepalive |
| 0x11 | `CLIENT_PONG` | C → S | Response to SERVER_PING |
| 0x1F | `BYE` | bidirectional | Graceful close |

### 1.2 Cognitive operations

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x20 | `ENCODE_REQ` | C → S | Encode a memory |
| 0xA0 | `ENCODE_RESP` | S → C | Encode result (memory_id) |
| 0x21 | `RECALL_REQ` | C → S | Recall memories matching a cue |
| 0xA1 | `RECALL_RESP` | S → C | Recall result (streaming) |
| 0x22 | `PLAN_REQ` | C → S | Plan from start to goal |
| 0xA2 | `PLAN_RESP` | S → C | Plan result (streaming) |
| 0x23 | `REASON_REQ` | C → S | Reason about an observation |
| 0xA3 | `REASON_RESP` | S → C | Reason result (streaming) |
| 0x24 | `FORGET_REQ` | C → S | Forget a memory |
| 0xA4 | `FORGET_RESP` | S → C | Forget result (acknowledgment) |
| 0x25 | `LINK_REQ` | C → S | Create an edge between two memories |
| 0xA5 | `LINK_RESP` | S → C | Link acknowledgment |
| 0x26 | `UNLINK_REQ` | C → S | Remove an edge between two memories |
| 0xA6 | `UNLINK_RESP` | S → C | Unlink acknowledgment |
| 0x2A | `ENCODE_VECTOR_DIRECT_REQ` | C → S | Power-user encode with pre-supplied vector |
| 0xAA | `ENCODE_VECTOR_DIRECT_RESP` | S → C | (Same response shape as ENCODE_RESP) |

### 1.3 Subscription

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x30 | `SUBSCRIBE_REQ` | C → S | Subscribe to memory events |
| 0xB0 | `SUBSCRIBE_EVENT` | S → C | Push event matching subscription |
| 0x31 | `UNSUBSCRIBE_REQ` | C → S | Stop a subscription |
| 0xB1 | `UNSUBSCRIBE_RESP` | S → C | Acknowledgment |

### 1.4 Transactions

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x40 | `TXN_BEGIN` | C → S | Begin transaction |
| 0xC0 | `TXN_BEGIN_RESP` | S → C | Confirm transaction id |
| 0x41 | `TXN_COMMIT` | C → S | Commit transaction |
| 0xC1 | `TXN_COMMIT_RESP` | S → C | Confirm commit |
| 0x42 | `TXN_ABORT` | C → S | Abort transaction |
| 0xC2 | `TXN_ABORT_RESP` | S → C | Confirm abort |

### 1.5 Stream control

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x50 | `CANCEL_STREAM` | C → S | Cancel an in-flight stream |
| 0xD0 | `CANCEL_STREAM_ACK` | S → C | Acknowledge cancellation |

### 1.6 Admin operations

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x60 | `ADMIN_STATS_REQ` | C → S | Request stats |
| 0xE0 | `ADMIN_STATS_RESP` | S → C | Stats response |
| 0x61 | `ADMIN_SNAPSHOT_REQ` | C → S | Take a snapshot |
| 0xE1 | `ADMIN_SNAPSHOT_RESP` | S → C | Snapshot result |
| 0x62 | `ADMIN_RESTORE_REQ` | C → S | Restore from snapshot |
| 0xE2 | `ADMIN_RESTORE_RESP` | S → C | Restore result |
| 0x63 | `ADMIN_INTEGRITY_CHECK_REQ` | C → S | Run integrity check |
| 0xE3 | `ADMIN_INTEGRITY_CHECK_RESP` | S → C | Integrity result |
| 0x64 | `ADMIN_MIGRATE_EMBEDDINGS_REQ` | C → S | Re-embed all memories |
| 0xE4 | `ADMIN_MIGRATE_EMBEDDINGS_RESP` | S → C | Migration progress (streaming) |
| 0x65 | `ADMIN_CREATE_CONTEXT_REQ` | C → S | Create a context with metadata |
| 0xE5 | `ADMIN_CREATE_CONTEXT_RESP` | S → C | Context creation ack |
| 0x66 | `ADMIN_RENAME_CONTEXT_REQ` | C → S | Rename a context |
| 0xE6 | `ADMIN_RENAME_CONTEXT_RESP` | S → C | Rename ack |
| 0x67 | `ADMIN_MOVE_MEMORY_REQ` | C → S | Move a memory between contexts |
| 0xE7 | `ADMIN_MOVE_MEMORY_RESP` | S → C | Move ack |
| 0x68 | `ADMIN_RECLASSIFY_REQ` | C → S | Change a memory's kind |
| 0xE8 | `ADMIN_RECLASSIFY_RESP` | S → C | Reclassify ack |
| 0x69 | `ADMIN_LIST_TOMBSTONED_REQ` | C → S | List tombstoned memories (debug) |
| 0xE9 | `ADMIN_LIST_TOMBSTONED_RESP` | S → C | List response (streaming) |

### 1.7 Errors

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0xFF | `ERROR` | bidirectional | Error frame; can be sent in response to any operation |

The error frame is a single opcode that carries an error code and details. See [`10_errors.md`](10_errors.md).

## 2. Reserved ranges

The following opcode ranges are reserved for future use:

- 0x70–0x7F (server-bound) — reserved for future client → server operations.
- 0xF0–0xFE (client-bound) — reserved for future server → client operations.

Receivers MUST treat unknown opcodes as protocol errors (sending `ERROR` with `BadOpcode`) — no silent discarding.

## 3. Symmetry between request and response

For most cognitive operations, the request opcode `0x2N` corresponds to the response opcode `0xAN`. Mnemonic: high bit set = response, low nibble selects the operation.

For admin operations, the pattern is `0x6N` → `0xEN`.

For connection management, the pattern is less regular because operations have multiple frames (PING/PONG, BYE bidirectional, etc.).

## 4. Operation dispatch

When the server receives a frame:

1. Validates the header (CRC, magic, version, reserved bytes).
2. Dispatches by opcode and stream_id.
3. For client → server opcodes (0x00–0x7F): processes the operation. Most operations carry a stream_id and the response uses the same stream_id.
4. For client-bound opcodes (0x80–0xFF): protocol error — clients shouldn't send these. The server responds with `ERROR(InvalidOpcode)`.

The reverse on the client side: the client expects only client-bound opcodes from the server.

## 5. Order of frames per opcode

### 5.1 Single-frame request → single-frame response

Examples: `ENCODE_REQ` → `ENCODE_RESP`, `FORGET_REQ` → `FORGET_RESP`.

```
client: REQ (stream_id=N, EOS)
server: RESP (stream_id=N, EOS)
```

The single frame in each direction carries the entire request/response. The stream is one frame long in each direction.

### 5.2 Single-frame request → streaming response

Examples: `RECALL_REQ` → multiple `RECALL_RESP` frames, similarly for `PLAN`, `REASON`.

```
client: REQ (stream_id=N, EOS)
server: RESP (stream_id=N, no EOS) [first results]
server: RESP (stream_id=N, no EOS) [more results]
...
server: RESP (stream_id=N, EOS)    [final batch or empty terminator]
```

The server emits intermediate frames as results become available; the EOS frame signals end of stream.

### 5.3 Subscription

```
client: SUBSCRIBE_REQ (stream_id=N, EOS)
server: SUBSCRIBE_EVENT (stream_id=N) [ongoing]
server: SUBSCRIBE_EVENT (stream_id=N) [as events occur]
...

(eventually:)
client: UNSUBSCRIBE_REQ (stream_id=M, EOS) referencing stream N
server: UNSUBSCRIBE_RESP (stream_id=M, EOS)
server: SUBSCRIBE_EVENT (stream_id=N, EOS) [final stream-end frame]
```

The unsubscribe is on a different stream; the original stream's EOS frame is sent when the unsubscribe completes.

### 5.4 Transaction

```
client: TXN_BEGIN (stream_id=N, EOS)
server: TXN_BEGIN_RESP (stream_id=N, EOS) [returns txn_id]

client: ENCODE_REQ (stream_id=M, EOS, with txn_id in payload)
server: ENCODE_RESP (stream_id=M, EOS) [memory buffered, not yet visible]

...more operations...

client: TXN_COMMIT (stream_id=K, EOS, txn_id)
server: TXN_COMMIT_RESP (stream_id=K, EOS) [commit applied]
```

Each operation in a transaction is its own stream. The transaction lifecycle has its own streams. The `txn_id` in the operation payload links them.

## 6. Flow examples

### 6.1 Simple ENCODE flow

```
[connection established, AUTH_OK received]

C → S: ENCODE_REQ(stream_id=1, EOS)
       payload: {text: "Hello world", context_id: 0, request_id: <uuid>}
S → C: ENCODE_RESP(stream_id=1, EOS)
       payload: {memory_id: <id>, status: ok}
```

### 6.2 Streaming RECALL flow

```
C → S: RECALL_REQ(stream_id=3, EOS)
       payload: {cue_text: "what about budgets", top_k: 5, ...}

S → C: RECALL_RESP(stream_id=3, !EOS)
       payload: {results: [r1, r2]}  (first batch streamed as ANN finds them)
S → C: RECALL_RESP(stream_id=3, !EOS)
       payload: {results: [r3]}
S → C: RECALL_RESP(stream_id=3, EOS)
       payload: {results: [r4, r5]}  (final batch, EOS)
```

The client may begin processing results as soon as the first frame arrives.

### 6.3 PING/PONG

```
C → S: PING(stream_id=0, EOS)
       payload: {client_timestamp: <ns>}
S → C: PONG(stream_id=0, EOS)
       payload: {client_timestamp: <ns>, server_timestamp: <ns>}
```

The client measures RTT from the timestamp difference.

## 7. Opcode evolution

Adding new opcodes is a wire-protocol-version bump (see [`12_versioning.md`](12_versioning.md)). The protocol's design accommodates additions:

- Reserved ranges (0x70–0x7F, 0xF0–0xFE) leave room.
- Existing opcodes are stable; their semantics don't change within a version.
- Negotiation at handshake gives both sides a chance to know what the other supports.

A future version 2 might add opcodes for replication-related operations, multi-modal operations, etc.

---

*Continue to [`06_handshake.md`](06_handshake.md) for the connection handshake.*
