# 04.01 Design and Transport

> **TL;DR.** Why Brain uses a custom binary framing over TCP with portable CBOR payloads (and not gRPC, REST, or a Rust-coupled zero-copy encoding), and what the transport layer looks like: TCP with optional TLS, default port 7474, connection lifecycle, keepalive, and TLS configuration. The framing is custom and tiny; the payloads are CBOR so any language can speak the protocol without a Brain-provided client.

## Design Choices

The wire protocol is a custom binary protocol over TCP. This section documents the alternatives considered and why they were rejected.


## 1. Why not gRPC

[gRPC](https://grpc.io/) is the obvious default for a typed RPC service in 2026. Brain considered it carefully and chose against it.

### 1.1 What gRPC would provide

- Mature ecosystem: code generation for ~10 languages, mature client libraries, observability integrations.
- Streaming model: bidirectional streaming maps cleanly to Brain's `RECALL`/`PLAN`/`REASON`/`SUBSCRIBE` pattern.
- Standard error codes, metadata propagation, deadlines.
- HTTP/2 underneath: connection multiplexing, header compression, well-understood flow control.

### 1.2 What gRPC would cost

The latency floor is the dominant cost. gRPC's stack is:

```
TCP → TLS → HTTP/2 frames → gRPC framing → Protobuf decode → handler
```

Each layer adds latency. On a sub-millisecond hot path, each microsecond matters:

- HTTP/2 frame parsing: ~5–20 µs.
- Protobuf decode of a typical `RECALL` request: ~10–50 µs (allocation-heavy on the standard generated path; less on hand-tuned).
- Combined gRPC overhead: ~30–100 µs per request before any work happens.

For a `RECALL` whose total budget is 10 ms (8 ms embedding + 2 ms everything else), 100 µs of gRPC overhead is 5% of the latency budget — eaten by framing alone. For cache-hit `RECALL` (target p50: 1.5 ms), it's 7%.

gRPC's other costs — a build-time `protoc` dependency, generated-client churn across ~10 languages, and an HTTP/2 stack Brain doesn't otherwise need — outweigh its ecosystem benefits for a protocol this small. Brain keeps its own framing and accepts a lightweight payload decode (CBOR) in exchange for portability: any language reads the payload with a stock library, and for Brain's text-in / results-out payloads the decode cost is single-digit microseconds. The zero-copy that a Rust-coupled encoding would buy is retained only where it pays off — internal storage (WAL, arena) and the trailing raw-vector section — not on the structured request/response bodies.

### 1.3 What Brain considered as a compromise

Brain looked at running gRPC on top of `rkyv`-encoded payloads (treating Protobuf message fields as opaque bytes containing rkyv data). This works mechanically but inherits gRPC's latency floor while losing Protobuf's ecosystem benefits. Worst of both.

Brain also considered using Cap'n Proto, which has zero-copy similar to rkyv. Cap'n Proto has its own RPC layer (Cap'n Proto RPC) but it's less mature than gRPC and adds its own complexity. Picking Cap'n Proto for the encoding only, on top of HTTP/2 framing, inherits most of gRPC's costs and none of its benefits.

### 1.4 The conclusion

For a system whose value proposition is latency, Brain keeps a custom binary framing over TCP — but it does **not** pay for that with a Rust-specific payload encoding. Payloads are CBOR ([RFC 8949](https://www.rfc-editor.org/rfc/rfc8949)), a self-describing format with a stock decoder in every language, so any third-party client can speak the protocol without a Brain-provided SDK. Brain ships none.

The trade-off is real but small:

- **Cost:** a client implements Brain's 32-byte framing (header, CRC32C, opcode dispatch). It is fully specified here and is a few dozen lines in any language.
- **Cost:** Brain implements its own observability hooks (gRPC's metadata propagation comes for free).
- **Benefit:** predictable, allocation-light framing on the hot path; portable payloads any language reads with an off-the-shelf CBOR library; zero-copy retained where it matters — the trailing raw-vector section ([`02_wire_format.md`](02_wire_format.md)) and all internal storage (WAL, arena), which keep their original layout.

The earlier rationale weighed gRPC's ~30–100 µs framing overhead against a Rust-coupled zero-copy payload (rkyv) and accepted that non-SDK clients couldn't connect. That accepted cost is now the thing the design avoids: a standalone database must be reachable from any language, and clients send **text** (Brain owns the embedding model), so a CBOR decode of a request body costs single-digit µs — not the 50–100 µs a vector-heavy decode would. Zero-copy on the structured payload was solving a problem this workload doesn't have.

The benefit is consequential at Brain's latency target. The cost is contained: the protocol is small (this spec defines all of it), the client surface is just this spec plus the conformance corpus (Brain ships no SDKs to maintain), and the protocol is unambiguous (Brain controls the spec).

## 2. Why not REST

REST over HTTP is a non-starter for this workload:

- **Latency.** HTTP/1.1 framing plus JSON encoding adds 100s of microseconds. HTTP/2 helps but still has the gRPC-equivalent overhead.
- **No persistent streams.** Each `RECALL` over REST is a separate request; long-running `PLAN`/`REASON`/`SUBSCRIBE` would need long-polling or Server-Sent Events.
- **JSON inefficiency.** Brain's payloads carry binary data (vectors, identifiers); base64-encoding for JSON adds 33% size and per-character overhead.
- **No native streaming.** Workarounds exist (chunked transfer, SSE) but none match the cleanliness of a binary streaming protocol.

REST is great for many things; it's not great for high-frequency low-latency typed RPC.

## 3. Why not UDP / QUIC

UDP-based protocols (raw UDP, QUIC, custom) optimize for situations where TCP's head-of-line blocking is problematic.

For Brain:

- **Frame ordering matters.** Operations are not independent — a `TXN_BEGIN` must precede operations within the transaction. Reordering frames at the protocol level is not an option.
- **Reliable delivery is required.** Lost frames must be retransmitted. Brain requires TCP's reliability or an equivalent.
- **HoL blocking is acceptable.** Within a single connection, requests share fate. Multiple connections are the answer for true independence; sharding handles cross-shard independence.

QUIC offers per-stream HoL avoidance over a UDP-based reliable transport. It would help when many independent streams share one connection. For Brain's pattern (per-shard connection, mostly-sequential operations within a stream), the benefit is small.

Brain uses TCP. A future major version could add QUIC support if real workloads benefit; the architecture doesn't preclude it.

## 4. Why not WebSockets

WebSockets are a TCP-based bidirectional framing on top of HTTP. For browser clients they're necessary. For server-to-server use, they layer on extra costs (HTTP upgrade, WebSocket framing) without benefits.

Brain's server-to-server target uses raw TCP framing. Browser clients are out of scope (Brain is a server, not a browser-side library); if ever needed, a WebSocket-tunneling proxy can wrap the protocol.

## 5. Why a 32-byte fixed header

Fixed-size headers simplify parsing:

- The reader knows in advance how many bytes to read for the header.
- Parsing the header is a single fixed-layout read; the CRC validates it before any field is trusted.
- The header CRC validates the header without needing the payload.

Variable-length headers would save a few bytes on small frames but complicate parsing and prevent zero-copy header access. The 32 bytes are enough room for all the fields Brain requires (magic, version, opcode, flags, header_crc32c, stream_id, payload_len, payload_crc32c) plus reserved space for one or two future expansions.

Brain considered 16 bytes (cuts header overhead in half) but ran out of room: with magic + version + opcode + flags + crc + stream_id + payload_len, the protocol already uses 19 bytes; adding payload_crc and reserved bytes pushed it to 32. The waste vs 16 bytes per frame is small (16 bytes of overhead) and worth the room for evolution.

## 6. Why split structured (CBOR) and raw vectors

Most payloads carry both structured fields (memory IDs, scores, metadata) and bulk binary data (vectors, embeddings). Brain splits them:

- **Structured fields** are encoded as a CBOR map. Portable — any language decodes it with a stock library, which is what lets Brain ship no client.
- **Raw vector bytes** are appended after the CBOR section as little-endian `f32`, located by an `offset`+`dim` field in the map. Zero-copy on read; no per-element encoding overhead.

This yields:

- Portable structured access (CBOR).
- Zero-copy bulk binary access (the trailing section is a plain `f32` array).
- One frame per logical message (no separate frames for "vector data").

The alternative — encoding vectors as a CBOR array of floats — would add per-element tag overhead. With 384-dim vectors that's 384 tagged elements per memory; the raw trailing section avoids it.

The alternative — sending vectors in a separate frame — would multiply round trips or require complex multi-frame messages.

The split keeps the structured part portable and the bulk part zero-copy. (Note: most clients never send vectors at all — Brain owns the embedding model, so clients send text. The raw section is the power-user `ENCODE_VECTOR_DIRECT` path.)

## 7. Why CRC32C, not stronger hashes

Each frame has two CRC32C checksums: one for the header, one for the payload.

CRC32C is:

- Fast — hardware-accelerated on x86 (SSE 4.2) and ARM (CRC32 extension).
- Adequate for detecting transmission errors — far stronger than a 16-bit CRC, more than enough for frame-level corruption detection.
- Not cryptographic — an adversary could forge a CRC32C. Brain's threat model is transmission errors, not adversarial corruption (TLS handles adversarial concerns).

Stronger hashes (BLAKE3, SHA-256) would be cryptographically secure but ~10× slower. For a per-frame check on the hot path, that's not acceptable.

## 8. Why bigtable-style stream IDs

Streams are identified by 32-bit integers, allocated by the client. This is similar to gRPC's stream model and several other protocols.

Why client-allocated:

- The client knows when to start a new stream (it's initiating the request).
- The server doesn't have to maintain a counter that's synchronized with the client.
- Client-allocated stream IDs are predictable and debuggable.

Why 32 bits:

- Allows ~4 billion concurrent streams per connection (effectively unlimited for any real workload).
- Fits in 4 bytes; small overhead.
- Client uses odd-numbered IDs; server uses even (reserved for future server-initiated streams; not currently used).

## 9. Wire opcode is `u16`, split into namespace + index

### Alternatives

(a) Keep `u8` opcode, place typed-graph ops in the gaps (`0x03–0x09`, `0x32–0x39`, etc.).
(b) Dispatch typed-graph ops behind a single substrate opcode (e.g. `0x70 KNOWLEDGE_OP`) with a sub-opcode byte in the body.
(c) Widen the opcode to `u16` and partition by high byte.

### Choice: (c).

### Reasoning

The original `u8` substrate opcode table was already dense (`0x20–0x29` cognitive, `0x30–0x31` subscribe, `0x40–0x42` txn, `0x50` cancel, `0x60–0x69` admin, `0x70–0x7F` reserved). The first typed-graph draft assigned `0x20–0x77` to entity / statement / relation / query ops — direct collision with substrate.

- (a) **renumber to fit gaps** loses the mnemonic (`0x3x = entity`) and produces non-contiguous family ranges. ~30+ typed-graph opcodes don't fit cleanly in the ~38 free bytes without spreading across `0x03`, `0x0F`, `0x27`, etc.
- (b) **sub-opcode under one substrate byte** adds one byte per typed-graph frame and a separate dispatch entry. Workable but always-on overhead even when reading a single `ENTITY_GET`.
- (c) **u16 with namespace prefix** is a one-time cost (the `u8 → u16` migration) for permanent clarity. Substrate ops kept their byte values (`ENCODE_REQ = 0x0020`); typed-graph ops live at `0x01xx`; future namespaces (statements-only, audit, etc.) have `0x02xx`–`0xFFxx` reserved.

The opcode width and the `flags` byte are the current wire shape; see [`03_opcodes.md`](03_opcodes.md).

### Cost paid

- 1 byte per frame.
- All existing substrate code call-sites updated in the same change (~256 sites across `brain-server`, tests).
- The `flags` field shrank from `u16` to `u8` to reclaim the byte. Only EOS / MPL / CMP bits were ever used; the shrink lost nothing.

## 10. Typed-graph errors ride the substrate ERROR frame

### Alternatives

(a) Typed-graph-namespace ops define their own `KNOWLEDGE_ERROR` (`0x01FF`) opcode with a separate body.
(b) Reuse Brain's `ERROR` (`0x00FF`) frame, extending `ErrorCodeWire` with typed-graph variants.

### Choice: (b).

### Reasoning

Two ERROR shapes mean clients write two error-handling paths. Reuse means one path with new enum variants — the cheapest extension point. The cost is coordinated edits to [`07_error_handling.md`](07_error_handling.md) when new typed-graph codes appear; at current scale the surface is small enough that this is fine.

Migration path detailed in [`07_error_handling.md`](07_error_handling.md) §3.10: an interim fallback (mapping new errors to closest existing substrate codes) lets handlers ship before `ErrorCodeWire` is extended; extension is the long-term goal.

## 11. SUBSCRIBE events extend the substrate event envelope

### Alternatives

(a) Typed-graph events use a separate opcode (`KNOWLEDGE_EVENT = 0x01B0`-ish) and parallel SUBSCRIBE channel.
(b) Typed-graph events extend Brain's `SubscriptionEvent` body with an optional `knowledge_payload` field.

### Choice: (b).

### Reasoning

Subscribers that want both substrate and typed-graph events would maintain two streams under (a) — twice the LSN tracking, twice the reconnect logic. (b) keeps a single per-shard LSN stream with optional typed payload. Schemaless subscribers ignore the optional field; typed-graph subscribers dispatch on it.

The cost is wire bandwidth — schemaless frames carry an extra 1-2 bytes for the `Option<KnowledgeEventPayload>::None` tag. Negligible.

## 12. Optional `WireUuid` fields use `[0; 16]` sentinel

### Alternatives

(a) A CBOR `null` (or an absent key) for "no id".
(b) Bare 16-byte string with the all-zeros value as "absent" sentinel.

### Choice: (b).

### Reasoning

A fixed `[u8; 16]` field is simpler for a client to read than a present-or-null union: the field is always there, always 16 bytes, and the all-zero value means absent. UUIDv7's first 48 bits are a unix-ms timestamp, so the all-zero value is unreachable — collision is impossible by construction. Keeping the field non-optional also keeps the per-opcode CBOR schemas ([`05_frame_layouts.md`](05_frame_layouts.md)) flat and the conformance vectors deterministic.

Used uniformly for `EntityId`, `StatementId`, `RelationId`, `AuditId`, etc., across the typed-graph wire shapes. Documented per-struct.

## 13. Replace-not-merge on `ENTITY_UPDATE` / `STATEMENT_*` collection fields

### Alternatives

(a) Wire shape carries deltas (add / remove lists for `aliases`, `properties`, etc.).
(b) Wire shape carries the full new state; server diffs against current.

### Choice: (b).

### Reasoning

Delta encoding lets clients send just the changed parts but doubles the protocol's surface area (every collection field needs add / remove / replace variants). Full-replace is simpler, matches the underlying `brain-metadata::entity_ops` API, and avoids edge cases (what if the delta references an alias the server has already removed?).

The cost is wire bandwidth on updates with many unchanged aliases. Acceptable until profiling shows otherwise.

## 14. Streaming reuses substrate per-frame model

### Alternatives

(a) Typed-graph-specific stream envelope: `STREAM_START` (metadata frame) → N × `STREAM_ITEM` → `STREAM_END`.
(b) Reuse Brain's per-frame model: each result is one frame with shared `stream_id`, EOS on the last.

### Choice: (b).

### Reasoning

Brain already implements (b) for `RECALL_RESP`, `PLAN_RESP`, `REASON_RESP`, `ADMIN_MIGRATE_EMBEDDINGS_RESP`, `ADMIN_LIST_TOMBSTONED_RESP`. Typed-graph list / query ops have the same shape (one logical result per frame, EOS terminates). Adding a separate envelope would mean two distinct streaming models in one server.

The original draft mentioned `STREAM_START` / `STREAM_ITEM` / `STREAM_END`; that's obsolete. See [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) R-K3.

## 15. Opaque attribute / property / evidence blobs

### Alternatives

(a) Wire structs decode attributes / properties into typed `BTreeMap<String, Value>` at the wire layer.
(b) Wire structs carry CBOR-encoded blobs; the schema validator unpacks them in the handler.

### Choice: (b).

### Reasoning

The schema isn't always known at the wire layer (the connection layer doesn't hold the schema registry). Pushing schema-aware decode into the wire forces a circular dependency or eager schema-replica caching at every dispatch point. Opaque blobs let the wire ship without knowing the schema; the handler (which holds `ctx.executor.metadata`) does the typed decode.

The cost is later error reporting on malformed attribute bags; tracked in [`../00_overview/04_open_questions_archive.md`](../00_overview/04_open_questions_archive.md) OQ-WP-K7.

## 16. Idempotency keying

### Choice

`(agent_id, opcode_u16, request_id, blake3(payload_bytes))` — the typed-graph opcode takes the same slot as any other opcode in the keying scheme. 24h TTL. See [`08_typed_graph_frames.md`](08_typed_graph_frames.md).

### Why not just `(agent_id, request_id)`?

Defends against a client recycling a `request_id` for a different operation. The `blake3(payload_bytes)` digest catches the "same id, different params" case as a structured `Conflict` rather than silently returning the stale cached response.

## 17. UUIDv7 everywhere for first-class IDs

### Choice

`EntityId`, `StatementId`, `RelationId`, `AuditId`, `MergeId`, `EvidenceOverflowId`, `request_id` are all UUIDv7. `EntityTypeId`, `RelationTypeId`, `PredicateId`, `ExtractorId` are `u32` interned ids.

### Reasoning

UUIDv7 is timestamp-prefixed → naturally sorts by creation order, which helps redb's b-tree locality. The all-zero sentinel (§12) is reachable only if a UUIDv7 implementation is broken. Interned u32 ids cover small-cardinality registries where the timestamp prefix would waste bytes.

## 18. Schema-optional is a deployment posture, not a degradation

### Choice

Schemaless deployments are **first-class**, not a "legacy" or "minimal" mode. Brain works fully without a schema; the typed graph is opt-in via `SCHEMA_UPLOAD`.

### Reasoning

Two product lines (vector substrate, knowledge graph) share a single codebase and a single deployment story. Operators that want only vectors don't pay for typed-graph features at all (no extractor budget, no LLM cache, no entity tables). Operators that want both flip one switch — declaring a schema via `SCHEMA_UPLOAD`.

## 19. Summary

The wire protocol's design choices:

- **Custom binary, not gRPC** — for latency.
- **TCP, not UDP/QUIC** — for ordering and reliability.
- **32-byte fixed header** — for parser simplicity.
- **CBOR + raw-vector split** — portable structured payloads (no SDK required), zero-copy bulk vectors.
- **CRC32C checksums** — for error detection without crypto cost.
- **Client-allocated 32-bit stream IDs** — for streaming model.
- **u16 opcode with namespace byte** — substrate at `0x00xx`, typed graph at `0x01xx`, future at `0x02xx+`.
- **One ERROR frame, one SUBSCRIBE envelope** — typed-graph errors and events ride substrate frames.
- **Opaque attribute / evidence blobs** — wire layer doesn't unpack schemas.
- **UUIDv7 for first-class IDs, u32 for interned registries.**

These choices trade ecosystem familiarity (gRPC, JSON) for performance and fit. The trade is justified by Brain's latency target.

---

## Transport Layer

The transport for Brain's wire protocol is TCP, optionally wrapped in TLS.


## 1. TCP

### 1.1 Default port

The IANA-assigned port for Brain is **`7474`**. (Subject to formal IANA assignment; this is the documented default.)

Operators MAY run Brain on a different port. Clients accept a server-supplied address and port from configuration.

### 1.2 TCP options

The server SHOULD set the following TCP options on accepted connections:

- `TCP_NODELAY` — disable Nagle's algorithm. Brain's frames are typically small and latency-sensitive; Nagle's batching adds milliseconds of latency for no benefit.
- `SO_KEEPALIVE` — enable TCP keepalive at the OS level. Recommended server defaults: **idle 75 s, interval 15 s, retries 9** (~210 s detection budget). This catches dead clients without application-level pings; the longer budget reflects that a single server tolerates many concurrent clients and shouldn't probe aggressively across all of them.
- `SO_REUSEADDR` (server only) — for graceful restart, allowing the server to rebind the listening socket.

Clients SHOULD set:

- `TCP_NODELAY` — same reason.
- `SO_KEEPALIVE` — to detect server crashes that don't close the connection cleanly. Recommended client defaults: **idle 30 s, interval 10 s, retries 3** (~60 s detection budget). Aggressive vs the server side because a client typically tracks one server, so faster probing is cheap; and operators want their next op to fail fast (and trigger transparent reconnect via the client's retry policy) rather than stall on a dead route. On platforms that don't expose the retries socket option (macOS, Windows), idle + interval still apply and the OS default retry count provides a slightly looser bound (~80 s).

### 1.3 Connection model

A single TCP connection can carry many concurrent operations, identified by stream IDs (see [`06_streaming.md`](06_streaming.md)). Clients SHOULD reuse connections rather than creating one per operation.

The server limits connections per agent (default: 100) and per-IP (default: 1000) to prevent abuse. Limits are configurable.

### 1.4 Connection lifecycle

```
Client                                    Server
  │                                         │
  │  TCP connect ────────────────────────►  │
  │  ◄──────────────────────── TCP accept   │
  │                                         │
  │  (optional: TLS handshake)              │
  │  TLS ClientHello ─────────────────────► │
  │  ◄──────────────── TLS ServerHello..    │
  │                                         │
  │  HELLO frame ──────────────────────────►│
  │  ◄────────────────────── WELCOME frame  │
  │                                         │
  │  AUTH frame ───────────────────────────►│
  │  ◄────────────────────── AUTH_OK frame  │
  │                                         │
  │     (now established; operations flow)  │
  │  ENCODE / RECALL / ... ───────────────► │
  │  ◄──────────────────────── ACK / data   │
  │                                         │
  │  ...                                    │
  │                                         │
  │  BYE ──────────────────────────────────►│
  │  ◄──────────────────────────────── BYE  │
  │  TCP close                              │
```

Detailed handshake flow in [`04_handshake.md`](04_handshake.md).

## 2. TLS

### 2.1 When to use TLS

TLS SHOULD be used whenever the connection traverses an untrusted network (Internet-facing deployments, multi-tenant infrastructure, etc.).

For internal-only deployments on a private network with no untrusted access, TLS is OPTIONAL. Operators may run Brain without TLS to save the handshake cost; they MUST be aware they're trading off confidentiality and integrity for ~1 ms of first-connection latency.

### 2.2 TLS version

Only **TLS 1.3** is supported. TLS 1.2 and earlier are refused.

This is a deliberate constraint: TLS 1.3 has clean security properties, simpler handshakes (1-RTT or 0-RTT), and fewer footguns. Limiting to 1.3 simplifies Brain's TLS configuration story significantly.

### 2.3 Cipher suites

TLS 1.3's mandatory cipher suites (per [RFC 8446](https://datatracker.ietf.org/doc/html/rfc8446)) are:

- `TLS_AES_128_GCM_SHA256`
- `TLS_AES_256_GCM_SHA384`
- `TLS_CHACHA20_POLY1305_SHA256`

The server MUST support at least the first two and the client MUST support at least one of them.

### 2.4 Certificate validation

By default, the client validates the server's certificate against the system trust store. Operators may configure:

- A custom trust anchor (for self-signed deployments).
- mTLS — both client and server present certificates.

Hostname verification SHOULD use the standard SAN (Subject Alternative Name) match per [RFC 6125](https://datatracker.ietf.org/doc/html/rfc6125).

### 2.5 SNI

Clients SHOULD send Server Name Indication (SNI) on connect. Servers may use SNI to route to multiple Brain instances behind a single TCP endpoint, though this is not the typical deployment.

### 2.6 ALPN

ALPN SHOULD use the protocol identifier `"brain/1"` for the current protocol version. This lets a TLS-terminating proxy distinguish Brain traffic from other protocols on the same port.

## 3. Connection establishment

### 3.1 Handshake budget

The full connect-and-handshake budget:

| Step | Typical | Worst case |
|---|---|---|
| TCP connect (3-way) | 0.5–1 ms (LAN) | varies |
| TLS handshake (1-RTT) | 1–2 ms (LAN, TLS 1.3) | 3–10 ms |
| HELLO/WELCOME | 0.1–0.5 ms | 1–2 ms |
| AUTH/AUTH_OK | 0.1–1 ms (token); 5–20 ms (mTLS verification, depends on cert chain) | varies |
| **Total to first operation** | **~3–5 ms (LAN, TLS, token auth)** | **~15–30 ms** |

The total is amortized across many subsequent operations on the same connection. A connection serving thousands of operations pays the handshake cost once.

### 3.2 Connection reuse

Clients SHOULD maintain a connection pool. The recommended client behavior:

- Pool size per server: 4–16 connections (configurable).
- Connections kept alive indefinitely; recycled on errors or after a max-idle time.
- Operations distributed across pool entries (round-robin or least-busy).

Per-operation connection creation is wasteful and not the recommended pattern.

## 4. Backpressure

The protocol uses TCP-level flow control for backpressure:

- When the server can't keep up, its receive buffer fills. The TCP window narrows. The client's writes block.
- When the client can't read fast enough, its receive buffer fills. The server's writes block.

There is no application-level flow control beyond this. Stream-level cancellation exists (see [`06_streaming.md`](06_streaming.md) §6) but doesn't slow the producer; it just stops the stream.

This is intentional. TCP flow control is well-understood and reliable; layering an application-level scheme on top adds complexity for little benefit.

## 5. Concurrency

### 5.1 Multiple streams per connection

Many operations can be in flight on one connection simultaneously. Each operation is a stream, identified by stream_id (see [`06_streaming.md`](06_streaming.md)).

The server processes streams concurrently within its per-shard concurrency limits. There's no per-connection sequencer that serializes them.

### 5.2 Frame interleaving

Frames from different streams may be interleaved on the wire. The reader demultiplexes by stream_id.

Frames within a single stream are sequential — the server emits them in order, and the client may rely on that order.

### 5.3 Out-of-order at the connection level

Within a single TCP connection, frames are ordered (TCP guarantees this). Out-of-order observation only happens across stream boundaries — stream A's frame N may be observed after stream B's frame M, even if B was started later. That's the point of streams: they're independent.

## 6. Idle behavior

### 6.1 Server idle timeout

If a connection is idle (no frames in or out) for more than the configured idle timeout (default: 5 minutes), the server SHOULD send a `PING` frame. If the client doesn't respond within the configured ping timeout (default: 30 seconds), the server closes the connection.

### 6.2 Client idle behavior

A client that's idle but wants to keep the connection alive SHOULD send periodic `PING` frames (default cadence: every 30 seconds). The server replies with `PONG`.

This is application-level keepalive, separate from TCP keepalive. TCP keepalive catches dead connections; application keepalive catches dead servers.

## 7. Graceful close

### 7.1 BYE frames

Either side can initiate close by sending a `BYE` frame. The recipient sends its own `BYE` and closes the TCP connection.

A `BYE` indicates "I'm done; finish what's in flight, then close". In-flight streams complete normally; no new streams may be initiated.

### 7.2 Abrupt close

A side that needs to close immediately just closes the TCP connection. The peer sees a connection error; in-flight operations fail with `ConnectionLost`.

This is the only path on emergency shutdown or panic conditions; otherwise, the graceful BYE flow is preferred.

## 8. Reconnection

### 8.1 Automatic reconnection

Clients SHOULD reconnect automatically on connection loss, with reasonable backoff (exponential, capped at ~30 seconds).

Reconnection re-establishes the connection from scratch: TCP, TLS, handshake. Cached connection state (session_id, agent context) is lost; the client must recreate it.

### 8.2 Stream resumption

In-flight streams cannot be resumed across reconnection. The exception is `SUBSCRIBE`, which carries a `from_lsn` parameter to resume the subscription from a specific log position. See [05. Operations](../05_operations/00_purpose.md) §SUBSCRIBE.

For other operations (`RECALL`, `PLAN`, etc.), the client retries from scratch with idempotency (where applicable via `request_id`) on the new connection.

## 9. Network constraints

### 9.1 MTU and fragmentation

Brain frames are typically under 1500 bytes (one Ethernet MTU); large `RECALL` results may exceed this. TCP handles segmentation; Brain does not worry about MTU at the application level.

For very large frames (>16 MiB), the protocol restricts payload size (see [`02_wire_format.md`](02_wire_format.md) §3.4); larger transfers must use streaming.

### 9.2 Latency tolerance

The protocol assumes sub-millisecond network latency to typical clients. WAN latency is supported but not optimized for; cross-region calls have correspondingly higher latency floors.

### 9.3 Bandwidth

Brain's typical workload is moderately bandwidth-intensive. See [01.05 Hardware](../01_architecture/05_hardware_and_targets.md) §5.2 for the bandwidth analysis.

## 10. IPv4 and IPv6

The server SHOULD listen on both IPv4 and IPv6 by default. Clients SHOULD prefer IPv6 when both are available (consistent with [RFC 6724](https://datatracker.ietf.org/doc/html/rfc6724) destination address selection).

---

*Continue to [`02_wire_format.md`](02_wire_format.md) for the frame format.*
