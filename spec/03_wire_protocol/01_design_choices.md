# 03.01 Design Choices

The wire protocol is a custom binary protocol over TCP. This file documents the alternatives considered and why they were rejected.

## 1. Why not gRPC

[gRPC](https://grpc.io/) is the obvious default for a typed RPC service in 2026. We considered it carefully and chose against it.

### 1.1 What gRPC would give us

- Mature ecosystem: code generation for ~10 languages, mature client libraries, observability integrations.
- Streaming model: bidirectional streaming maps cleanly to our `RECALL`/`PLAN`/`REASON`/`SUBSCRIBE` pattern.
- Standard error codes, metadata propagation, deadlines.
- HTTP/2 underneath: connection multiplexing, header compression, well-understood flow control.

### 1.2 What gRPC would cost us

The latency floor is the dominant cost. gRPC's stack is:

```
TCP → TLS → HTTP/2 frames → gRPC framing → Protobuf decode → handler
```

Each layer adds latency. On a sub-millisecond hot path, each microsecond matters:

- HTTP/2 frame parsing: ~5–20 µs.
- Protobuf decode of a typical `RECALL` request: ~10–50 µs (allocation-heavy on the standard generated path; less on hand-tuned).
- Combined gRPC overhead: ~30–100 µs per request before any work happens.

For a `RECALL` whose total budget is 10 ms (8 ms embedding + 2 ms everything else), 100 µs of gRPC overhead is 5% of the latency budget — eaten by framing alone. For cache-hit `RECALL` (target p50: 1.5 ms), it's 7%.

There's a second, less-visible cost. Protobuf's wire format is a serialization step: the data on the wire isn't directly usable by the program; it has to be decoded into language-native objects. Our payloads are already typed Rust structs; we want them on the wire in a form we can directly access without copying.

This is what `rkyv` provides (zero-copy structured deserialization) and Protobuf does not.

### 1.3 What we considered as a compromise

We looked at running gRPC on top of `rkyv`-encoded payloads (treating Protobuf message fields as opaque bytes containing rkyv data). This works mechanically but inherits gRPC's latency floor while losing Protobuf's ecosystem benefits. Worst of both.

We also considered using Cap'n Proto, which has zero-copy similar to rkyv. Cap'n Proto has its own RPC layer (Cap'n Proto RPC) but it's less mature than gRPC and adds its own complexity. Picking Cap'n Proto for the encoding only, on top of HTTP/2 framing, gives us most of gRPC's costs and none of its benefits.

### 1.4 The conclusion

For a system whose value proposition is latency, we accept the cost of a custom binary protocol. The trade-off is real:

- **Cost:** we maintain protocol implementations in each language SDK, instead of using a generated client.
- **Cost:** we implement our own observability hooks (gRPC's metadata propagation comes for free).
- **Cost:** clients without a Brain SDK can't connect.
- **Benefit:** ~50–100 µs latency reduction per request, predictable framing, zero-copy payload access.

The benefit is consequential at our latency target. The cost is contained: the protocol is small (this spec defines all of it), the SDK count is small (4 languages in v1), and the protocol is unambiguous (we control the spec).

## 2. Why not REST

REST over HTTP is a non-starter for this workload:

- **Latency.** HTTP/1.1 framing plus JSON encoding adds 100s of microseconds. HTTP/2 helps but still has the gRPC-equivalent overhead.
- **No persistent streams.** Each `RECALL` over REST is a separate request; long-running `PLAN`/`REASON`/`SUBSCRIBE` would need long-polling or Server-Sent Events.
- **JSON inefficiency.** Our payloads carry binary data (vectors, identifiers); base64-encoding for JSON adds 33% size and per-character overhead.
- **No native streaming.** Workarounds exist (chunked transfer, SSE) but none match the cleanliness of a binary streaming protocol.

REST is great for many things; it's not great for high-frequency low-latency typed RPC.

## 3. Why not UDP / QUIC

UDP-based protocols (raw UDP, QUIC, custom) optimize for situations where TCP's head-of-line blocking is problematic.

For Brain:

- **Frame ordering matters.** Operations are not independent — a `TXN_BEGIN` must precede operations within the transaction. Reordering frames at the protocol level is not an option.
- **Reliable delivery is required.** Lost frames must be retransmitted. We need TCP's reliability or an equivalent.
- **HoL blocking is acceptable.** Within a single connection, requests share fate. Multiple connections are the answer for true independence; sharding handles cross-shard independence.

QUIC offers per-stream HoL avoidance over a UDP-based reliable transport. It would help if we had many independent streams over one connection. For our pattern (per-shard connection, mostly-sequential operations within a stream), the benefit is small.

We chose TCP. Future versions could add QUIC support if real workloads benefit; the architecture doesn't preclude it.

## 4. Why not WebSockets

WebSockets are a TCP-based bidirectional framing on top of HTTP. For browser clients they're necessary. For server-to-server use, they layer on extra costs (HTTP upgrade, WebSocket framing) without benefits.

Brain's server-to-server target uses raw TCP framing. Browser clients are out of scope (Brain is a server, not a browser-side library); if ever needed, a WebSocket-tunneling proxy can wrap the protocol.

## 5. Why a 32-byte fixed header

Fixed-size headers simplify parsing:

- The reader knows in advance how many bytes to read for the header.
- Parsing is a single struct cast (zero-copy) under bytemuck/rkyv.
- The header CRC validates the header without needing the payload.

Variable-length headers would save a few bytes on small frames but complicate parsing and prevent zero-copy header access. The 32 bytes are enough room for all the fields we need (magic, version, opcode, flags, header_crc32c, stream_id, payload_len, payload_crc32c) plus reserved space for one or two future expansions.

We considered 16 bytes (cuts header overhead in half) but ran out of room: with magic + version + opcode + flags + crc + stream_id + payload_len, we already use 19 bytes; adding payload_crc and reserved bytes pushed us to 32. The waste vs 16 bytes per frame is small (16 bytes of overhead) and worth the room for evolution.

## 6. Why split rkyv (structured) and bytemuck (raw vectors)

Most payloads carry both structured fields (memory IDs, scores, metadata) and bulk binary data (vectors, embeddings). We split:

- **Structured fields** are encoded with rkyv. Zero-copy on read; small overhead per field.
- **Raw vector bytes** are appended after the rkyv-encoded structure, accessed via bytemuck's `cast_slice<u8, f32>`. Zero-copy on read; no encoding overhead.

This gives us:

- Zero-copy structured access (rkyv).
- Zero-copy bulk binary access (bytemuck).
- One frame per logical message (no separate frames for "vector data").

The alternative — encoding vectors as repeated `f32` fields in rkyv — would add per-element overhead. With 384-dim vectors, that's 384 fields per memory; the overhead matters.

The alternative — sending vectors in a separate frame — would multiply round trips or require complex multi-frame messages.

The split keeps frames atomic and zero-copy throughout.

## 7. Why CRC32C, not stronger hashes

Each frame has two CRC32C checksums: one for the header, one for the payload.

CRC32C is:

- Fast — hardware-accelerated on x86 (SSE 4.2) and ARM (CRC32 extension).
- Adequate for detecting transmission errors — far stronger than a 16-bit CRC, more than enough for frame-level corruption detection.
- Not cryptographic — an adversary could forge a CRC32C. Our threat model is transmission errors, not adversarial corruption (TLS handles adversarial concerns).

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
- Client uses odd-numbered IDs; server uses even (reserved for future server-initiated streams, not used in v1).

## 9. Summary

The wire protocol's design choices:

- **Custom binary, not gRPC** — for latency.
- **TCP, not UDP/QUIC** — for ordering and reliability.
- **32-byte fixed header** — for parser simplicity.
- **rkyv + bytemuck split** — for zero-copy of both structured and raw data.
- **CRC32C checksums** — for error detection without crypto cost.
- **Client-allocated 32-bit stream IDs** — for streaming model.

These choices trade ecosystem familiarity (gRPC, JSON) for performance and fit. The trade is justified by Brain's latency target.

---

*Continue to [`02_transport.md`](02_transport.md) for the transport layer.*
