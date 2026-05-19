# 02 — Wire protocol

**Audience:** anyone implementing an SDK in a new language,
debugging a transport-level bug, or thinking about adding an
opcode.

**Goal:** by the end you can write a hex dump of a `Frame`, name
every field, point to the code that validates each one, and
explain what guarantees an arriving frame buys before its payload
is parsed.

This chapter assumes you've read
[01 — System architecture](01-system-architecture.md) (so "frame"
already means something) and [03 — Arena and WAL](03-arena-and-wal.md)
(so when we mention a `MemoryId` you know which file's slot it's
talking about).

---

## What the wire is

Brain speaks a custom binary protocol over TCP. There is no HTTP
or gRPC at the data plane — those are saved for the admin and
metrics endpoints. The reason is latency: the substrate's whole
budget for a `RECALL` is tens of milliseconds, and any
text-shaped envelope eats hundreds of microseconds before the
server has even decided which shard to talk to.

The shape of a wire interaction:

```
client                                              server
  │                                                    │
  │  TCP open ──────────────────────────────────────► │
  │  (optional TLS handshake)                          │
  │                                                    │
  │  HELLO frame ───────────────────────────────────► │
  │ ◄─────────────────────────────────── WELCOME frame│
  │                                                    │
  │  AUTH frame ────────────────────────────────────► │
  │ ◄─────────────────────────────────── AUTH_OK frame│
  │                                                    │
  │  ENCODE_REQ frame ───────────────────────────────►│
  │ ◄────────────────────────────────── ENCODE_RESP …
  │                                                    │
  │  RECALL_REQ frame ───────────────────────────────►│
  │ ◄────────────────────────────────── RECALL_RESP …
  │                                                    │
  │  …                                                 │
  │                                                    │
  │  BYE ──────────────────────────────────────────► │
  │  TCP close                                         │
```

Every interaction past the TLS handshake is a sequence of
**frames**. A frame is a 32-byte header followed by `payload_len`
bytes of payload. Some opcodes only have a header; many have a
short rkyv-encoded structured body; a few carry trailing raw
bytes (vector blobs).

`brain-protocol` (the crate) is *pure*: no I/O, no async, no
runtime dependency. It encodes and decodes bytes, validates them,
and stops there. The connection layer
([01](01-system-architecture.md)) is what runs it over TCP.

`#![forbid(unsafe_code)]` is set at the crate root
(`crates/brain-protocol/src/lib.rs:14`). Wire parsing is the
attack surface; no `unsafe` is the policy.

---

## The frame header, byte by byte

The header is exactly 32 bytes, `repr(C, packed)`, all multi-byte
fields **big-endian**
(`crates/brain-protocol/src/header.rs:41`):

```
 byte  0  1  2  3  4  5  6  7  8  9 10 11 12 13 14 15
      ┌────────────┬──┬─────┬──┬────────────┬───────────┐
      │  magic     │v │ op  │f │ header_crc │ stream_id │
      │ "BRN0"     │  │     │  │   (CRC32C) │           │
      └────────────┴──┴─────┴──┴────────────┴───────────┘
 byte 16 17 18 19 20 21 22 23 24 25 26 27 28 29 30 31
      ┌────────┬──┬────────────┬─────────────────────┐
      │ pay_len│ra│payload_crc │ reserved_b (8 × 0)  │
      │ (u24)  │  │  (CRC32C)  │                     │
      └────────┴──┴────────────┴─────────────────────┘
```

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | `magic` | `b"BRN0"` — `MAGIC` constant in `lib.rs:39`. |
| 4 | 1 | `version` | Wire version. `1` in v1. |
| 5 | 2 | `opcode` | Big-endian `u16`. High byte = namespace. |
| 7 | 1 | `flags` | EOS (0x80), MPL (0x40), CMP (0x20). Low 5 bits reserved (must be 0). |
| 8 | 4 | `header_crc32c` | CRC32C over header bytes `[0..8]` followed by `[12..32]` — i.e. the rest of the header with this field treated as zero. |
| 12 | 4 | `stream_id` | Big-endian `u32`. Multiplexing key. |
| 16 | 3 | `payload_len` | 24-bit big-endian. Hard cap 16 MiB − 1. |
| 19 | 1 | `reserved_a` | Must be zero. |
| 20 | 4 | `payload_crc32c` | CRC32C over the payload bytes. Zero when payload is empty. |
| 24 | 8 | `reserved_b` | Eight bytes of zero. |

The size and alignment are static-asserted at the type
(`crates/brain-protocol/src/header.rs:69`):

```rust
const _: () = {
    assert!(core::mem::size_of::<Header>() == 32);
    assert!(core::mem::align_of::<Header>() == 1);
};
```

Alignment is 1 because `repr(C, packed)`. That lets us cast a
32-byte slice straight to `Header` with no padding or copies via
`bytemuck::cast`
(`crates/brain-protocol/src/frame.rs:97`).

### Why big-endian, when the arena is little-endian?

The arena ([03](03-arena-and-wal.md)) is little-endian because
that's native on x86 and ARM and it lets the SIMD code read f32
vectors with no byte-swapping. The wire is big-endian — the
network-byte-order tradition — so an integer in a hex dump reads
left-to-right, which makes the protocol much easier to inspect
with `tcpdump` or `wireshark`. The two endiannesses are converted
at the wire boundary; they never mix elsewhere.

### Why a `u24` payload length?

24 bits is enough for 16 MiB − 1, which is the spec-mandated hard
cap on a single payload. A frame that wants to carry more is a
protocol error. Larger payloads (think: an admin snapshot
manifest) use the multi-payload flag (`MPL`) to span multiple
frames with the same `stream_id` and an `EOS` on the last one.
This is the same mechanism used for streaming responses
(`SUBSCRIBE_EVENT`, `RECALL_RESP` when it overflows).

### The two CRCs

There are two CRC32C fields, not one, for one specific reason:
**you can decide whether to read the payload at all before
allocating for it.** The header has its own CRC over its own
bytes; if that fails, the frame is malformed and the connection
must be closed before any payload-size-bounded allocation. Only
after the header validates do we trust `payload_len` enough to
bound the read.

The `header_crc32c` covers bytes `[0..8] ++ [12..32]` —
everything except itself. The trick of treating the CRC field as
zero during the computation lets `seal()` write the bytes in
place
(`crates/brain-protocol/src/header.rs:134`,
`crates/brain-protocol/src/header.rs:202`).

Both CRCs are computed with `crc32c::crc32c`
(`crates/brain-protocol/src/crc.rs:28`) — Castagnoli polynomial,
the same as ext4 and SCTP, picked because it's accelerated by
modern CPUs and gives stronger guarantees against burst errors
than CRC32-IEEE.

### Flags

Three bits are defined, in `flags` byte:

| Bit | Constant | Meaning |
|---|---|---|
| `0x80` | `EOS` | End of stream — the last frame for this `stream_id`. |
| `0x40` | `MPL` | Multi-payload — this frame is part of a larger logical message. |
| `0x20` | `CMP` | Compressed payload (zstd). Reserved; not used in v1. |

The low five bits are `FLAGS_RESERVED_MASK`
(`crates/brain-protocol/src/header.rs:82`). Any one of them set
on the wire is a `ReservedFieldNonZero` rejection.

### `stream_id`

Streams are how Brain multiplexes many in-flight operations on
one TCP connection. Every request frame carries a `stream_id`;
the response on the same `stream_id` is the reply. A new
request picks a `stream_id` the client isn't currently using;
streaming responses (subscribe events, paginated recalls) emit
multiple frames with the same id and finish on `EOS`.

`stream_id = 0` is reserved for connection-level frames
(`HELLO`, `WELCOME`, `AUTH`, `AUTH_OK`, server-initiated
`PING`/`PONG`). Application requests start at 1.

---

## Validating a header

`Header::validate`
(`crates/brain-protocol/src/header.rs:141`) is one of the most
load-bearing routines in the protocol. Every byte that arrives
runs through it. The check sequence:

1. **`magic == b"BRN0"`** — fast reject for arbitrary garbage.
2. **`version == VERSION`** (`= 1`) — major-version gate.
3. **`reserved_a == 0` and `reserved_b == [0; 8]`** — forward
   compatibility marker.
4. **`flags & FLAGS_RESERVED_MASK == 0`** — the low five bits
   must be zero.
5. **`payload_len ≤ MAX_PAYLOAD_BYTES`** — 24-bit cap.
6. **`header_crc32c` matches the computed CRC** over
   `[0..8] ++ [12..32]`.

The order matters: cheap checks before expensive ones. We
reject most garbage on the magic; the CRC is computed only on
plausible-looking headers.

The full `Frame::decode_with_max`
(`crates/brain-protocol/src/frame.rs:84`) layers on top:

```rust
pub fn decode_with_max(bytes, max_payload_bytes) {
    if bytes.len() < HEADER_SIZE          → Truncated
    header = bytemuck::cast(bytes[..32])
    header.validate()?                    // the six checks above
    payload_len = header.payload_len_u32()
    if payload_len > max_payload_bytes    → OversizePayload
    if bytes.len() < HEADER_SIZE + payload_len → Truncated
    if computed_payload_crc != stored     → BadPayloadCrc
    return Frame { header, payload: bytes.to_vec() }
}
```

The `max_payload_bytes` parameter is what the connection layer
negotiated at `HELLO`/`WELCOME` — it can be tighter than the
spec's 16 MiB hard cap. The bound is checked *before* the
payload is read so an attacker can't claim a 16 MiB length on a
truncated socket and force a large allocation
(`crates/brain-protocol/src/frame.rs:101`).

### A property the test suite enforces

`Frame::decode` is a *total function* on arbitrary bytes
(`crates/brain-protocol/src/frame.rs:311`): it either returns a
structured `ProtocolError` or it succeeds — and on success, the
consumed prefix must re-encode to exactly itself. There is no
input on which it panics, hangs, or returns approximate data.
That's enforced by a proptest, not just convention.

---

## Opcodes and namespaces

Opcodes are `u16` and split into namespaces by the high byte
(`crates/brain-protocol/src/opcode.rs:31`):

| High byte | Namespace | Defined |
|---|---|---|
| `0x00xx` | Substrate (cognitive primitives, connection, admin) | yes |
| `0x01xx` | Knowledge layer (entities, statements, schema, …) | yes |
| `0x02xx`–`0xFFxx` | Reserved | — |

Within a namespace, the low byte's high bit is direction: low
byte `< 0x80` is a request (client → server), `≥ 0x80` is a
response. The pairing is by-convention: `0x0020 ENCODE_REQ` ↔
`0x00A0 ENCODE_RESP`; `0x0130 ENTITY_CREATE_REQ` ↔
`0x01B0 ENTITY_CREATE_RESP`.

A flavour of the substrate set (`crates/brain-protocol/src/opcode.rs:36`):

```
Connection mgmt   0x0001 HELLO        ↔ 0x0081 WELCOME
                  0x0002 AUTH         ↔ 0x0082 AUTH_OK
                  0x0010 PING         ↔ 0x0090 PONG
                  0x0091 SERVER_PING  ↔ 0x0011 CLIENT_PONG
                  0x001F BYE
Cognitive ops     0x0020 ENCODE       ↔ 0x00A0 ENCODE_RESP
                  0x0021 RECALL       ↔ 0x00A1 RECALL_RESP
                  0x0022 PLAN         ↔ 0x00A2 PLAN_RESP
                  0x0023 REASON       ↔ 0x00A3 REASON_RESP
                  0x0024 FORGET       ↔ 0x00A4 FORGET_RESP
                  0x0025 LINK         ↔ 0x00A5 LINK_RESP
                  0x0026 UNLINK       ↔ 0x00A6 UNLINK_RESP
                  0x002A ENCODE_VECTOR_DIRECT ↔ 0x00AA …
Subscriptions     0x0030 SUBSCRIBE    ↔ 0x00B0 SUBSCRIBE_EVENT
                  0x0031 UNSUBSCRIBE  ↔ 0x00B1 UNSUBSCRIBE_RESP
Transactions      0x0040 TXN_BEGIN    ↔ 0x00C0 TXN_BEGIN_RESP
                  0x0041 TXN_COMMIT   ↔ 0x00C1 TXN_COMMIT_RESP
                  0x0042 TXN_ABORT    ↔ 0x00C2 TXN_ABORT_RESP
Stream control    0x0050 CANCEL_STREAM ↔ 0x00D0 CANCEL_STREAM_ACK
Admin             0x0060…0x0069  ↔ 0x00E0…0x00E9
Errors            0x00FF ERROR
```

The knowledge-layer set (active only when a schema is declared,
covered in [09](09-knowledge-layer.md) and [11](11-hybrid-retrieval-rrf.md))
includes `ENTITY_*` (`0x0130–0x0138`), `STATEMENT_*`,
`RELATION_*`, `SCHEMA_*` (`0x0120–0x0123`), `EXTRACTOR_*`
(`0x0124–0x0126`), and the hybrid `QUERY` / `RECALL_HYBRID` /
`QUERY_EXPLAIN` / `QUERY_TRACE` family. Every wire response in
this family follows the same `_REQ` ↔ `_RESP` convention.

---

## Payload encoding

Payloads come in two shapes.

### Structured payloads — rkyv 0.7

Most opcodes carry a structured body. We use [rkyv 0.7](https://docs.rs/rkyv/0.7)
because it gives us two properties that matter on the hot path:

- **Zero-copy decode.** `rkyv::check_archived_root` validates the
  bytes against the schema and returns a `&ArchivedT` straight
  into the wire buffer — no allocation, no field-by-field copy.
- **Forward-only schema.** Adding a new optional field is
  source-compatible; the wire bytes for the old layout still
  decode.

We *do* deserialise to an owned `T` after validation
(`crates/brain-protocol/src/rkyv_codec.rs:38`):

```rust
let archived = rkyv::check_archived_root::<T>(bytes)?;
archived.deserialize(&mut Infallible)
```

The reason is that handlers later in the pipeline outlive the
wire buffer — they may need to `.await`, drop the buffer, and
keep working. For the connection-level paths that *don't*
outlive the buffer (PING, header reads), we cast directly with
`bytemuck` and skip the rkyv step entirely.

Encoding goes through `AllocSerializer<256>`
(`crates/brain-protocol/src/rkyv_codec.rs:24`). The 256-byte
initial scratch covers short messages without an immediate
realloc; rkyv grows it as needed. Encoding can't fail for our
body types (no I/O, just allocation), so the helper unwraps the
unreachable path.

The high-level entry points are the matched enums:

- `RequestBody::encode`
  (`crates/brain-protocol/src/request.rs:215`) and
  `RequestBody::decode(opcode, bytes)`
  (`crates/brain-protocol/src/request.rs:286`).
- `ResponseBody::encode`
  (`crates/brain-protocol/src/response.rs:233`) and
  `ResponseBody::decode(opcode, bytes)`
  (`crates/brain-protocol/src/response.rs:303`).

`decode` takes the opcode as an input because the body is
opcode-discriminated: the same bytes parse differently as an
`Encode` vs a `Recall` request. The `Opcode` lives in the
header; the body in the payload; the two are joined at the
decode call site.

### Raw payloads — `bytemuck` views

Vector blobs (the 1536-byte `f32` array for an embedding) are
*not* rkyv-encoded. They sit either as the trailing portion of a
mixed-encoding payload (for `ENCODE_VECTOR_DIRECT` or for a
`RECALL` whose cue is a pre-computed vector) or as the
exclusive payload of a no-structured-fields frame.

The reason is that rkyv would have to wrap an `Vec<f32>` with an
archived length-prefix and validate it. `bytemuck` can cast a
`&[u8]` straight to `&[f32]` with one alignment check and no
copying. Vectors are flat arrays of f32s on the wire (LE inside
the bytes); the slot in the arena ([03](03-arena-and-wal.md)) is
the same layout. Sending and receiving an embedding is therefore
a memcpy.

### Wire-domain types

Request and response structs use raw representations rather than
`brain-core` types directly
(`crates/brain-protocol/src/request.rs:37`):

| Wire type | Domain type |
|---|---|
| `[u8; 16]` (`WireUuid`) | `AgentId`, `RequestId`, `TxnId` |
| `u128` (`WireMemoryId`) | `MemoryId` (shard, slot, version, reserved) |
| `u64` (`WireContextId`) | `ContextId` |
| `u8`-mapped `enum` | `MemoryKind`, `EdgeKind`, `AuthMethod`, … |

The split is deliberate. `brain-core` shouldn't depend on rkyv
(rkyv is a wire concern), and rkyv shouldn't dictate the shape
of internal types (the internal types want stronger invariants).
Conversion happens at the handler boundary.

---

## The handshake

The first two round-trips after TCP/TLS open are negotiation,
not application traffic. They establish the protocol version
both sides agree on, the server's parameters, and the agent's
identity.

```
   client                                          server
     │                                                │
     │  HELLO (stream 0)                              │
     │    supported_versions: [1]                     │
     │    capabilities: { streaming: true, … }        │
     │    client_id: "my-app/0.4"                     │
     │ ─────────────────────────────────────────────►│
     │                                                │
     │                                                │  negotiate()
     │                                                │   pick max common version
     │                                                │   AND capability flags
     │                                                │
     │ ◄───────────────────────── WELCOME (stream 0) │
     │    chosen_version: 1                           │
     │    capabilities: { streaming: true, … }        │
     │    server_features: {                          │
     │      max_payload_size: 16 MiB,                 │
     │      max_concurrent_streams: 1024,             │
     │      idle_timeout_seconds: 300,                │
     │      auth_methods: [Token, None]               │
     │    }                                           │
     │                                                │
     │  AUTH (stream 0)                               │
     │    credentials: Token("…") | Mtls(…) | None    │
     │ ─────────────────────────────────────────────►│
     │                                                │
     │                                                │  validate
     │                                                │   credentials against
     │                                                │   advertised methods
     │                                                │
     │ ◄───────────────────────── AUTH_OK (stream 0) │
     │    agent_id: UUID                              │
     │    permissions: { can_encode, can_recall, … }  │
     │                                                │
     │  any request opcode (stream ≥ 1) …            │
```

`negotiate`
(`crates/brain-protocol/src/handshake.rs:262`) picks the highest
mutually-supported version and ANDs the capability flags. Auth
method intersection is **not** part of negotiation — the server
just *advertises* its accepted methods in `WELCOME`, and rejects
unsupported methods at AUTH time (gives a `NoSuchAuthMethod`
error).

The auth credentials are intentionally typed:

```rust
enum AuthCredentials {
    Token(Vec<u8>),
    Mtls(MtlsClaim),  // SHA-256 cert fingerprint + asserted subject
    None,
}
```

(`crates/brain-protocol/src/handshake.rs:77`)

`None` is for trusted-network and dev deployments. The server
ships with `auth = none` by default; production should switch
to `Token` (post-Phase-9) or `Mtls` and front the
metrics/admin HTTP endpoints with their own auth.

After `AUTH_OK`, the connection enters the `Established` state
([01 — System architecture](01-system-architecture.md)) and any
opcode in the agent's permission set is fair game on streams
≥ 1.

---

## Errors

The protocol has a single typed error frame: `ERROR (0x00FF)`.
Its payload carries an `ErrorCode` (one of ~50 named values in
`crates/brain-protocol/src/error.rs:70`) and a category. Examples
of the kinds:

| Category | Members |
|---|---|
| `Protocol` | `BadMagic`, `BadHeaderCrc`, `BadPayloadCrc`, `BadOpcode`, `BadVersion`, `OversizePayload`, `ReservedFieldNonZero`, `MalformedRkyv`, `MalformedVector` |
| `Authentication` | `Unauthenticated`, `NoSuchAuthMethod`, `SessionExpired` |
| `Authorization` | `PermissionDenied`, `AdminPermissionRequired`, `WrongShard` |
| `Validation` | `InvalidArgument`, `MissingRequiredField`, `TextTooLarge`, `BadContextId`, `TopKOutOfRange`, … |
| `NotFound` | `MemoryNotFound`, `ContextNotFound`, `SubscriptionNotFound`, `SnapshotNotFound`, `TxnNotFound` |
| `Conflict` | `IdempotencyConflict`, `TransactionConflict`, `StreamIdInUse`, … |
| `ResourceExhausted` | `OutOfSlots`, `OutOfDisk`, `OutOfMemory`, `RateLimited`, `ConnectionLimitExceeded`, … |
| `Internal` | `StorageError`, `IndexError`, `EmbeddingError`, `MetadataError` |
| `Unavailable` | `ShardUnavailable`, `Overloaded`, `Restarting`, `Maintenance` |

The category drives client retry behaviour:
`ErrorCategory::is_retryable`
(`crates/brain-protocol/src/error.rs:52`) returns `true` for
`ResourceExhausted`, `Internal`, and `Unavailable`. Everything
else is a client-side problem the client must fix before retry
would help.

Two operational rules follow from the taxonomy:

- **Protocol errors close the connection.** Once a frame is
  malformed, the byte stream's framing is suspect. The
  connection task emits an `ERROR` frame with the relevant
  `Protocol` code and closes the socket.
- **Other errors keep the connection alive.** A
  `MemoryNotFound` or a `RateLimited` is a per-request failure;
  the stream gets an `ERROR` reply, but the next request on a
  fresh `stream_id` is fine.

---

## A walkthrough: encoding a `RECALL_REQ`

To make the layering concrete, here's exactly what bytes leave
the client when it issues a 2-NN recall over a text cue
`"old houses"`.

```
1. Build RequestBody::Recall(RecallRequest { … })
       ─ agent_id: [u8; 16]
       ─ context_id: u64
       ─ cue: Cue::Text("old houses")
       ─ top_k: 2
       ─ filters, hints, request_id, …
   ↓
2. body_bytes = body.encode()                       // rkyv
       ─ rkyv::AllocSerializer<256> serialises every field
       ─ result: short Vec<u8>, e.g. 96 bytes
   ↓
3. Frame::new(opcode = 0x0021, flags = 0, stream_id = 1, body_bytes)
       ─ header.magic = b"BRN0"
       ─ header.version = 1
       ─ header.opcode = [0x00, 0x21]
       ─ header.flags = 0
       ─ header.stream_id = [0,0,0,1]
       ─ header.payload_len = [0,0,96]
       ─ header.payload_crc32c = crc32c(body_bytes)
       ─ header.header_crc32c = crc32c(header[0..8] ++ header[12..32])
   ↓
4. frame.encode()                                   // serialise to bytes
       ─ 32-byte header + 96-byte body = 128 bytes
   ↓
5. socket.write_all(&bytes)
```

On the server side the dance reverses:

```
1. socket.read_exact(buf, 32)         // first the header
   ↓
2. header = bytemuck::cast(buf)
3. header.validate()?                 // six checks (above)
   ↓
4. payload_len = header.payload_len_u32()      // 96
5. socket.read_exact(buf, payload_len)         // exactly 96 more bytes
6. payload_crc check                  // against header.payload_crc32c
   ↓
7. RequestBody::decode(Opcode::RecallReq, body_bytes)
       ─ rkyv::check_archived_root::<RecallRequest>(body_bytes)
       ─ archived.deserialize(&mut Infallible) → owned RecallRequest
   ↓
8. dispatcher → shard owning agent_id    (chapter 01)
```

For a `RECALL` whose cue is a 1536-byte pre-computed vector
instead of text, step 2 (`body.encode()`) produces the rkyv
header part as before, then the raw vector bytes are appended.
The shard's handler splits them back into "rkyv structured part"
and "raw vector blob" using the explicit length field in the
structured part.

---

## Multi-payload and streaming frames

Two flag bits handle messages bigger than one frame:

- **`MPL` (0x40) — multi-payload.** A single logical message
  too big for 16 MiB sends a series of frames with the same
  `stream_id`, all carrying `MPL`. The final frame sets `EOS`
  (and may or may not also carry `MPL`); the reassembler
  concatenates payloads in order.
- **`EOS` (0x80) — end of stream.** Used both as the trailing
  marker on multi-payload single messages and as the
  end-of-stream signal for true streaming opcodes
  (`SUBSCRIBE_EVENT`, paginated `RECALL_RESP`).

A streaming `SUBSCRIBE` flow:

```
client → SUBSCRIBE_REQ      (stream 7)
server ← SUBSCRIBE_EVENT    (stream 7)     ← first event …
server ← SUBSCRIBE_EVENT    (stream 7)     ← more events as they happen
server ← SUBSCRIBE_EVENT    (stream 7)
… (over time) …
client → UNSUBSCRIBE_REQ    (stream 7)
server ← UNSUBSCRIBE_RESP   (stream 7, EOS)
```

Or:

```
client → SUBSCRIBE_REQ      (stream 7)
server ← SUBSCRIBE_EVENT    (stream 7)
client → CANCEL_STREAM      (stream 7)
server ← CANCEL_STREAM_ACK  (stream 7, EOS)
```

After `EOS` on a stream, the `stream_id` is *retired* for the
lifetime of the connection — reusing it is `StreamIdInUse`.

Two limits worth knowing:

- **`max_concurrent_streams`** (default 1024 from `WELCOME`) —
  the highest number of distinct stream ids that may be
  in flight at once on a single connection. The server enforces
  the limit; over it, requests get `StreamLimitExceeded`.
- **`idle_timeout_seconds`** (default 300) — if the connection
  is quiet for this long, the server initiates a `SERVER_PING`
  and expects a `CLIENT_PONG` within a few seconds. Failing
  that, the connection is closed.

---

## Versioning and the namespace plan

The wire protocol version is the `version` byte in every header.
v1 is what's documented here. A future v2 ships when there is no
forward-compatible way to add what we need — a removed opcode, a
field-type change, a new flag bit. The version field is the only
thing that lets a v2 server keep serving v1 clients while a v2
client connects to a v1 server gets `BadVersion`.

What's not a major-version event:

- **Adding a new opcode.** Old clients never send the opcode and
  old servers reply with `BadOpcode`. Source-compatible.
- **Adding a new field to a body.** rkyv keeps it
  source-compatible as long as the new field is optional /
  defaulted.
- **A new namespace.** The high byte of the opcode is unused;
  using a new high-byte value doesn't affect existing parsers.

What *is* a major-version event:

- Changing the header layout (size, field order, endianness).
- Changing the magic.
- Removing or repurposing an existing opcode.
- Changing a body's field type incompatibly.

There's an internal review skill that fires automatically on
edits to `header.rs` / `opcode.rs` / `frame.rs` / body files for
exactly this reason — adding a new field is fine, but any
non-additive change needs to be a deliberate choice, not an
accident.

---

## Failure modes

What can go wrong, what the server does, and what the client sees.

**Bad magic.** The first 4 bytes aren't `BRN0`. Either it isn't
a Brain client, or the byte stream is corrupted past the framing.
The server emits an `ERROR(BadMagic)` and closes. (Some clients
won't even see the error if they sent total garbage.)

**Version mismatch.** Header's `version` byte doesn't match the
server's. `ERROR(BadVersion)`, connection closes. The
human-facing message includes the server's expected version.

**Header CRC mismatch.** A bit flipped in transit, or the
header was constructed wrong by the client. `ERROR(BadHeaderCrc)`,
close. The header is the framing — once it's suspect, byte
alignment for subsequent frames is suspect too.

**Payload CRC mismatch.** The payload arrived corrupted.
`ERROR(BadPayloadCrc)`, close. (We *could* keep the connection
open since framing is intact, but the safer policy is to treat
any CRC failure as "trust the byte stream less.")

**Reserved field non-zero.** Either a client bug or an attempt
to use an undefined flag bit. `ERROR(ReservedFieldNonZero)`,
close.

**Oversize payload.** `payload_len` exceeds the negotiated
maximum (16 MiB by default). `ERROR(OversizePayload)`, close —
otherwise we'd have to read those bytes off the socket to keep
framing, and we'd rather not.

**Unknown opcode.** The header validates but the opcode isn't
one we recognise. `ERROR(BadOpcode)`, the stream gets the error,
but the **connection stays open** — this is per-request, not a
framing failure.

**Malformed rkyv payload.** Header and CRCs pass, but the body
bytes don't validate as the schema for the opcode.
`ERROR(MalformedRkyv)`, per-stream error, connection stays open.

**Out-of-order frame after `EOS`.** A client reuses a retired
`stream_id`. `ERROR(StreamIdInUse)`, per-request.

**Stream over the concurrency limit.** Over
`max_concurrent_streams` simultaneously open ids.
`ERROR(StreamLimitExceeded)`, per-request, retryable after
something closes.

---

## Configuration & tuning

Most of the wire layer's knobs are negotiated at handshake, not
configured statically:

| Knob | Source | Default | Notes |
|---|---|---|---|
| `max_payload_size` | `WELCOME.server_features` | 16 MiB | Server's hard cap. Connection-layer config can shrink it. |
| `max_concurrent_streams` | `WELCOME.server_features` | 1024 | Per connection. Larger = more memory per connection. |
| `idle_timeout_seconds` | `WELCOME.server_features` | 300 | Lower = more wakeup traffic on quiet links. |
| `auth_methods` | `WELCOME.server_features` | `[None]` | v1 dev default. Production should switch to `[Token]` or `[Mtls]`. |
| TLS enabled | `server.tls.enabled` (TOML) | `false` | `cert` and `key` paths required when enabled. |

A few standing rules:

- **Don't ship `auth = none` to the internet.** The connection
  layer trusts whatever AGENT id the client claims in `AUTH`
  when no credentials are demanded.
- **Pick `idle_timeout` higher than your load-balancer's idle
  timeout, not lower.** Otherwise the LB closes the socket and
  Brain logs surprising disconnects.
- **Bigger `max_concurrent_streams` is mostly free until you
  hit per-connection memory.** Each stream costs roughly an
  in-flight `Frame` worth of buffer plus a reply oneshot.
- **TLS is rustls.** No openssl dependency. mTLS works the same
  way — the server presents a cert and the client may present
  one too, which the server fingerprints into the `MtlsClaim`.

---

## Where it lives in the code

| Topic | Path |
|---|---|
| Top-level re-exports, constants, `MAGIC`, `HEADER_SIZE`, `MAX_PAYLOAD_BYTES` | `crates/brain-protocol/src/lib.rs` |
| Header layout + validate + seal | `crates/brain-protocol/src/header.rs` |
| Frame envelope, encode, decode, oversize check | `crates/brain-protocol/src/frame.rs` |
| CRC32C helpers | `crates/brain-protocol/src/crc.rs` |
| Opcode enum, namespaces | `crates/brain-protocol/src/opcode.rs` |
| Request body enum + rkyv encode/decode | `crates/brain-protocol/src/request.rs` |
| Response body enum + rkyv encode/decode | `crates/brain-protocol/src/response.rs` |
| rkyv codec helpers (`check_archived_root` + `Infallible`) | `crates/brain-protocol/src/rkyv_codec.rs` |
| Per-op request structs | `crates/brain-protocol/src/requests/` |
| Per-op response structs | `crates/brain-protocol/src/responses/` |
| Handshake payloads + `negotiate` | `crates/brain-protocol/src/handshake.rs` |
| Knowledge-layer wire types | `crates/brain-protocol/src/knowledge/` |
| Error taxonomy | `crates/brain-protocol/src/error.rs` |

---

## Further reading

- [01 — System architecture](01-system-architecture.md) for what
  the connection layer does *with* a decoded frame and how it
  picks a shard.
- [03 — Arena and WAL](03-arena-and-wal.md) for where a
  `MemoryId` returned in an `ENCODE_RESP` actually points.
- [09 — Knowledge layer](09-knowledge-layer.md) for when the
  `0x01xx` namespace opcodes are accepted at all.
- [11 — Hybrid retrieval](11-hybrid-retrieval-rrf.md) for the
  `QUERY` / `RECALL_HYBRID` family in the knowledge namespace.
