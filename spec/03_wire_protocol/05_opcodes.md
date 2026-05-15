# 03.05 Opcodes

The opcode is a big-endian `u16` in the frame header (bytes 5–6). The high byte is a **namespace** (`0x00` substrate — this section; `0x01` knowledge layer — see [`../28_knowledge_wire_protocol/00_purpose.md`](../28_knowledge_wire_protocol/00_purpose.md); `0x02–0xFF` reserved). Within a namespace the low byte's high bit selects direction: low byte `< 0x80` is server-bound (C → S); low byte `≥ 0x80` is client-bound (S → C).

> **Pre-v1.0 wire change (phase 16.6a):** the opcode widened from `u8` to `u16` and `flags` shrank from `u16` to `u8` (see [`03_frame_header.md`](03_frame_header.md) §1). The substrate's numeric assignments below kept their low byte intact — `ENCODE_REQ` is still `0x20`, now read as `0x0020`. See [`12_versioning.md`](12_versioning.md) for the compatibility policy this falls under.

## 1. The complete table

### 1.1 Connection management

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0001 | `HELLO` | C → S | Initial frame; client identity and supported versions |
| 0x0081 | `WELCOME` | S → C | Reply to HELLO; server identity, negotiated version, session_id |
| 0x0002 | `AUTH` | C → S | Authentication credentials |
| 0x0082 | `AUTH_OK` | S → C | Authentication success; bind to agent_id |
| 0x0010 | `PING` | C → S | Keepalive |
| 0x0090 | `PONG` | S → C | Response to PING |
| 0x0091 | `SERVER_PING` | S → C | Server-initiated keepalive |
| 0x0011 | `CLIENT_PONG` | C → S | Response to SERVER_PING |
| 0x001F | `BYE` | bidirectional | Graceful close |

### 1.2 Cognitive operations

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0020 | `ENCODE_REQ` | C → S | Encode a memory |
| 0x00A0 | `ENCODE_RESP` | S → C | Encode result (memory_id) |
| 0x0021 | `RECALL_REQ` | C → S | Recall memories matching a cue |
| 0x00A1 | `RECALL_RESP` | S → C | Recall result (streaming) |
| 0x0022 | `PLAN_REQ` | C → S | Plan from start to goal |
| 0x00A2 | `PLAN_RESP` | S → C | Plan result (streaming) |
| 0x0023 | `REASON_REQ` | C → S | Reason about an observation |
| 0x00A3 | `REASON_RESP` | S → C | Reason result (streaming) |
| 0x0024 | `FORGET_REQ` | C → S | Forget a memory |
| 0x00A4 | `FORGET_RESP` | S → C | Forget result (acknowledgment) |
| 0x0025 | `LINK_REQ` | C → S | Create an edge between two memories |
| 0x00A5 | `LINK_RESP` | S → C | Link acknowledgment |
| 0x0026 | `UNLINK_REQ` | C → S | Remove an edge between two memories |
| 0x00A6 | `UNLINK_RESP` | S → C | Unlink acknowledgment |
| 0x002A | `ENCODE_VECTOR_DIRECT_REQ` | C → S | Power-user encode with pre-supplied vector |
| 0x00AA | `ENCODE_VECTOR_DIRECT_RESP` | S → C | (Same response shape as ENCODE_RESP) |

### 1.3 Subscription

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0030 | `SUBSCRIBE_REQ` | C → S | Subscribe to memory events |
| 0x00B0 | `SUBSCRIBE_EVENT` | S → C | Push event matching subscription |
| 0x0031 | `UNSUBSCRIBE_REQ` | C → S | Stop a subscription |
| 0x00B1 | `UNSUBSCRIBE_RESP` | S → C | Acknowledgment |

### 1.4 Transactions

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0040 | `TXN_BEGIN` | C → S | Begin transaction |
| 0x00C0 | `TXN_BEGIN_RESP` | S → C | Confirm transaction id |
| 0x0041 | `TXN_COMMIT` | C → S | Commit transaction |
| 0x00C1 | `TXN_COMMIT_RESP` | S → C | Confirm commit |
| 0x0042 | `TXN_ABORT` | C → S | Abort transaction |
| 0x00C2 | `TXN_ABORT_RESP` | S → C | Confirm abort |

### 1.5 Stream control

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0050 | `CANCEL_STREAM` | C → S | Cancel an in-flight stream |
| 0x00D0 | `CANCEL_STREAM_ACK` | S → C | Acknowledge cancellation |

### 1.6 Admin operations

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x0060 | `ADMIN_STATS_REQ` | C → S | Request stats |
| 0x00E0 | `ADMIN_STATS_RESP` | S → C | Stats response |
| 0x0061 | `ADMIN_SNAPSHOT_REQ` | C → S | Take a snapshot |
| 0x00E1 | `ADMIN_SNAPSHOT_RESP` | S → C | Snapshot result |
| 0x0062 | `ADMIN_RESTORE_REQ` | C → S | Restore from snapshot |
| 0x00E2 | `ADMIN_RESTORE_RESP` | S → C | Restore result |
| 0x0063 | `ADMIN_INTEGRITY_CHECK_REQ` | C → S | Run integrity check |
| 0x00E3 | `ADMIN_INTEGRITY_CHECK_RESP` | S → C | Integrity result |
| 0x0064 | `ADMIN_MIGRATE_EMBEDDINGS_REQ` | C → S | Re-embed all memories |
| 0x00E4 | `ADMIN_MIGRATE_EMBEDDINGS_RESP` | S → C | Migration progress (streaming) |
| 0x0065 | `ADMIN_CREATE_CONTEXT_REQ` | C → S | Create a context with metadata |
| 0x00E5 | `ADMIN_CREATE_CONTEXT_RESP` | S → C | Context creation ack |
| 0x0066 | `ADMIN_RENAME_CONTEXT_REQ` | C → S | Rename a context |
| 0x00E6 | `ADMIN_RENAME_CONTEXT_RESP` | S → C | Rename ack |
| 0x0067 | `ADMIN_MOVE_MEMORY_REQ` | C → S | Move a memory between contexts |
| 0x00E7 | `ADMIN_MOVE_MEMORY_RESP` | S → C | Move ack |
| 0x0068 | `ADMIN_RECLASSIFY_REQ` | C → S | Change a memory's kind |
| 0x00E8 | `ADMIN_RECLASSIFY_RESP` | S → C | Reclassify ack |
| 0x0069 | `ADMIN_LIST_TOMBSTONED_REQ` | C → S | List tombstoned memories (debug) |
| 0x00E9 | `ADMIN_LIST_TOMBSTONED_RESP` | S → C | List response (streaming) |

### 1.7 Errors

| Opcode | Name | Direction | Purpose |
|---|---|---|---|
| 0x00FF | `ERROR` | bidirectional | Error frame; can be sent in response to any operation |

The error frame is a single opcode that carries an error code and details. See [`10_errors.md`](10_errors.md).

## 2. Reserved ranges

Within the substrate namespace (`0x00xx`), the following low-byte ranges are reserved:

- 0x70–0x7F (server-bound, `0x0070–0x007F`) — reserved for future C → S substrate operations.
- 0xF0–0xFE (client-bound, `0x00F0–0x00FE`) — reserved for future S → C substrate operations.

Other namespaces (`0x01xx` knowledge, etc.) have their own reservations — see the corresponding spec sections.

Receivers MUST treat unknown opcodes as protocol errors (sending `ERROR` with `BadOpcode`) — no silent discarding.

## 3. Symmetry between request and response

For most cognitive operations, the request opcode `0x002N` corresponds to the response opcode `0x00AN`. Mnemonic: low byte's high bit set = response, low nibble selects the operation.

For admin operations, the pattern is `0x006N` → `0x00EN`.

Knowledge-layer opcodes follow the same convention within their `0x01xx` namespace: e.g. `0x0130 ENTITY_CREATE` (request) ↔ `0x01B0 ENTITY_CREATE_RESP`.

For connection management, the pattern is less regular because operations have multiple frames (PING/PONG, BYE bidirectional, etc.).

## 4. Operation dispatch

When the server receives a frame:

1. Validates the header (CRC, magic, version, reserved bytes).
2. Dispatches by opcode and stream_id.
3. For server-bound opcodes (low byte `< 0x80`, in any namespace): processes the operation. Most operations carry a stream_id and the response uses the same stream_id.
4. For client-bound opcodes (low byte `≥ 0x80`): protocol error — clients shouldn't send these. The server responds with `ERROR(InvalidOpcode)`.

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
