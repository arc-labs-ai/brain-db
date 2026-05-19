# Wire opcodes

Complete enumeration. Encoded as `u16` big-endian in the frame
header (bytes 5–6). High byte = namespace, low byte = op index.

**Source:** `crates/brain-protocol/src/opcode.rs`.
**Spec:** §03/05 (substrate), §28 (knowledge layer).

## Direction encoding

Within each namespace, opcodes are paired request/response:

- Low byte `< 0x80` → request (client → server).
- Low byte `≥ 0x80` → response (server → client).

A request opcode `0x00<N>` pairs with response opcode `0x00<N | 0x80>`
(e.g. `EncodeReq = 0x0020`, `EncodeResp = 0x00A0`).

## Substrate namespace (`0x00xx`)

### Connection management

| Hex | Name | Dir | Notes |
|---|---|---|---|
| `0x0001` | `Hello` | C→S | First frame on a new connection. |
| `0x0081` | `Welcome` | S→C | Reply to HELLO; carries session_id and negotiated features. |
| `0x0002` | `Auth` | C→S | Authentication credentials. |
| `0x0082` | `AuthOk` | S→C | Auth success; binds agent_id. |
| `0x0010` | `Ping` | C→S | Keepalive. |
| `0x0090` | `Pong` | S→C | Response to PING. |
| `0x0091` | `ServerPing` | S→C | Server-initiated keepalive. |
| `0x0011` | `ClientPong` | C→S | Response to SERVER_PING. |
| `0x001F` | `Bye` | C↔S | Graceful close. |

See [`handshake.md`](handshake.md) for the establishment dance.

### Cognitive operations

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0020` | `EncodeReq` | C→S | Store a memory. |
| `0x00A0` | `EncodeResp` | S→C | Returns the new `MemoryId`. |
| `0x0021` | `RecallReq` | C→S | Retrieve memories by similarity. |
| `0x00A1` | `RecallResp` | S→C | Streaming results. |
| `0x0022` | `PlanReq` | C→S | Bidirectional BFS through the memory graph. |
| `0x00A2` | `PlanResp` | S→C | Streaming paths. |
| `0x0023` | `ReasonReq` | C→S | Find supporting / contradicting evidence. |
| `0x00A3` | `ReasonResp` | S→C | Streaming evidence. |
| `0x0024` | `ForgetReq` | C→S | Soft or hard delete. |
| `0x00A4` | `ForgetResp` | S→C | Per-id ack. |
| `0x0025` | `LinkReq` | C→S | Create an edge. |
| `0x00A5` | `LinkResp` | S→C | Edge ack. |
| `0x0026` | `UnlinkReq` | C→S | Remove an edge. |
| `0x00A6` | `UnlinkResp` | S→C | Edge removal ack. |
| `0x002A` | `EncodeVectorDirectReq` | C→S | Power-user encode with pre-supplied vector. |
| `0x00AA` | `EncodeVectorDirectResp` | S→C | Same shape as `EncodeResp`. |

See [`../cognitive-operations/`](../cognitive-operations/) for
field-level semantics.

### Subscription

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0030` | `SubscribeReq` | C→S | Open a change-event stream. |
| `0x00B0` | `SubscribeEvent` | S→C | One event per frame; long-lived stream. |
| `0x0031` | `UnsubscribeReq` | C→S | Close a subscription. |
| `0x00B1` | `UnsubscribeResp` | S→C | Ack. |

### Transactions

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0040` | `TxnBegin` | C→S | Begin a multi-op transaction. |
| `0x00C0` | `TxnBeginResp` | S→C | Returns `txn_id`. |
| `0x0041` | `TxnCommit` | C→S | Commit a transaction. |
| `0x00C1` | `TxnCommitResp` | S→C | Commit ack. |
| `0x0042` | `TxnAbort` | C→S | Abort a transaction. |
| `0x00C2` | `TxnAbortResp` | S→C | Abort ack. |

### Stream control

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0050` | `CancelStream` | C→S | Cancel an in-flight stream. |
| `0x00D0` | `CancelStreamAck` | S→C | Ack. |

### Admin (substrate)

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0060` | `AdminStatsReq` | C→S | Request internal stats. |
| `0x00E0` | `AdminStatsResp` | S→C | Stats payload. |
| `0x0061` | `AdminSnapshotReq` | C→S | Create a snapshot. |
| `0x00E1` | `AdminSnapshotResp` | S→C | Snapshot ack. |
| `0x0062` | `AdminRestoreReq` | C→S | Restore from a snapshot. |
| `0x00E2` | `AdminRestoreResp` | S→C | Restore ack. |
| `0x0063` | `AdminIntegrityCheckReq` | C→S | Run an integrity check. |
| `0x00E3` | `AdminIntegrityCheckResp` | S→C | Result. |
| `0x0064` | `AdminMigrateEmbeddingsReq` | C→S | Re-embed all memories under a new model. |
| `0x00E4` | `AdminMigrateEmbeddingsResp` | S→C | Streaming progress. |
| `0x0065` | `AdminCreateContextReq` | C→S | Create a context. |
| `0x00E5` | `AdminCreateContextResp` | S→C | Ack. |
| `0x0066` | `AdminRenameContextReq` | C→S | Rename a context. |
| `0x00E6` | `AdminRenameContextResp` | S→C | Ack. |
| `0x0067` | `AdminMoveMemoryReq` | C→S | Move a memory between contexts. |
| `0x00E7` | `AdminMoveMemoryResp` | S→C | Ack. |
| `0x0068` | `AdminReclassifyReq` | C→S | Change a memory's kind. |
| `0x00E8` | `AdminReclassifyResp` | S→C | Ack. |
| `0x0069` | `AdminListTombstonedReq` | C→S | List tombstoned memories (debug). |
| `0x00E9` | `AdminListTombstonedResp` | S→C | Streaming list. |

### Errors

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x00FF` | `Error` | C↔S | Generic error frame; bound to a stream_id. |

### Reserved (substrate)

- `0x0070–0x007F` — future substrate requests.
- `0x00F0–0x00FE` — future substrate responses.

Sending an unassigned opcode returns `BadOpcode`.

## Knowledge namespace (`0x01xx`)

Active when a schema has been declared via `SchemaUploadReq`.

### Schema

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0120` | `SchemaUploadReq` | C→S | Upload a DSL document. |
| `0x01A0` | `SchemaUploadResp` | S→C | Validation errors or new schema version. |
| `0x0121` | `SchemaGetReq` | C→S | Retrieve a specific schema version. |
| `0x01A1` | `SchemaGetResp` | S→C | Schema document. |
| `0x0122` | `SchemaListReq` | C→S | Version history for a namespace. |
| `0x01A2` | `SchemaListResp` | S→C | Streaming list. |
| `0x0123` | `SchemaValidateReq` | C→S | Dry-run parse + validate. |
| `0x01A3` | `SchemaValidateResp` | S→C | Errors or would-be version. |
| `0x0124` | `ExtractorListReq` | C→S | List registered extractors. |
| `0x01A4` | `ExtractorListResp` | S→C | List. |
| `0x0125` | `ExtractorDisableReq` | C→S | Disable an extractor. |
| `0x01A5` | `ExtractorDisableResp` | S→C | Ack. |
| `0x0126` | `ExtractorEnableReq` | C→S | Re-enable an extractor. |
| `0x01A6` | `ExtractorEnableResp` | S→C | Ack. |

### Entities

| Hex | Name | Dir |
|---|---|---|
| `0x0130` | `EntityCreateReq` | C→S |
| `0x01B0` | `EntityCreateResp` | S→C |
| `0x0131` | `EntityGetReq` | C→S |
| `0x01B1` | `EntityGetResp` | S→C |
| `0x0132` | `EntityUpdateReq` | C→S |
| `0x01B2` | `EntityUpdateResp` | S→C |
| `0x0133` | `EntityRenameReq` | C→S |
| `0x01B3` | `EntityRenameResp` | S→C |
| `0x0134` | `EntityMergeReq` | C→S |
| `0x01B4` | `EntityMergeResp` | S→C |
| `0x0135` | `EntityUnmergeReq` | C→S |
| `0x01B5` | `EntityUnmergeResp` | S→C |
| `0x0136` | `EntityResolveReq` | C→S |
| `0x01B6` | `EntityResolveResp` | S→C |
| `0x0137` | `EntityListReq` | C→S |
| `0x01B7` | `EntityListResp` | S→C |
| `0x0138` | `EntityTombstoneReq` | C→S |
| `0x01B8` | `EntityTombstoneResp` | S→C |

### Statements

| Hex | Name | Dir |
|---|---|---|
| `0x0140` | `StatementCreateReq` | C→S |
| `0x01C0` | `StatementCreateResp` | S→C |
| `0x0141` | `StatementGetReq` | C→S |
| `0x01C1` | `StatementGetResp` | S→C |
| `0x0142` | `StatementSupersedeReq` | C→S |
| `0x01C2` | `StatementSupersedeResp` | S→C |
| `0x0143` | `StatementTombstoneReq` | C→S |
| `0x01C3` | `StatementTombstoneResp` | S→C |
| `0x0144` | `StatementRetractReq` | C→S |
| `0x01C4` | `StatementRetractResp` | S→C |
| `0x0145` | `StatementHistoryReq` | C→S |
| `0x01C5` | `StatementHistoryResp` | S→C |
| `0x0146` | `StatementListReq` | C→S |
| `0x01C6` | `StatementListResp` | S→C |

### Relations

| Hex | Name | Dir |
|---|---|---|
| `0x0150` | `RelationCreateReq` | C→S |
| `0x01D0` | `RelationCreateResp` | S→C |
| `0x0151` | `RelationGetReq` | C→S |
| `0x01D1` | `RelationGetResp` | S→C |
| `0x0152` | `RelationSupersedeReq` | C→S |
| `0x01D2` | `RelationSupersedeResp` | S→C |
| `0x0153` | `RelationTombstoneReq` | C→S |
| `0x01D3` | `RelationTombstoneResp` | S→C |
| `0x0154` | `RelationListFromReq` | C→S |
| `0x01D4` | `RelationListFromResp` | S→C |
| `0x0155` | `RelationListToReq` | C→S |
| `0x01D5` | `RelationListToResp` | S→C |
| `0x0156` | `RelationTraverseReq` | C→S |
| `0x01D6` | `RelationTraverseResp` | S→C |

### Hybrid queries

| Hex | Name | Dir | One-liner |
|---|---|---|---|
| `0x0160` | `QueryReq` | C→S | Execute a structured hybrid query. |
| `0x01E0` | `QueryResp` | S→C | Streaming results. |
| `0x0161` | `QueryExplainReq` | C→S | Explain plan. |
| `0x01E1` | `QueryExplainResp` | S→C | Explanation. |
| `0x0162` | `QueryTraceReq` | C→S | Per-stage trace. |
| `0x01E2` | `QueryTraceResp` | S→C | Streaming trace events. |
| `0x0163` | `RecallHybridReq` | C→S | Substrate `RecallReq` shape, routed through the hybrid engine. |
| `0x01E3` | `RecallHybridResp` | S→C | Streaming. |

## See also

- [`frame-format.md`](frame-format.md) — how an opcode is packed into a frame.
- [`error-codes.md`](error-codes.md) — what an `Error(0x00FF)` payload looks like.
- [`../cognitive-operations/`](../cognitive-operations/) — semantics of substrate ops.

**Spec:** §03/05 (substrate), §28 (knowledge wire protocol). **Source:** `crates/brain-protocol/src/opcode.rs`.
