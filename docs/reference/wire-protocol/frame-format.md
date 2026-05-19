# Wire frame format

Brain's binary wire protocol. Every byte that crosses
`listen_addr` is a frame.

**Source:** `crates/brain-protocol/src/{header,frame,lib}.rs`.
**Spec:** §03/03 (frame header), §03/04 (payload encoding).

## Constants

| Constant | Value | Source |
|---|---|---|
| Magic | `b"BRN0"` (0x42 0x52 0x4E 0x30) | `lib.rs:39`, `header.rs:44` |
| Protocol version | `1` | `header.rs:34` |
| Header size | `32` bytes (`#[repr(C, packed)]`) | `header.rs:70` |
| Max payload | `16_777_215` bytes (2²⁴ − 1) | `lib.rs:45` |
| Endianness | Big-endian (header), little-endian (vector bytes) | spec §03/03 §1 |
| CRC | CRC32C (Castagnoli polynomial 0x1EDC6F41) | spec §03/03 §3.6 |

## Header layout (32 bytes)

```
 0               4   5    6   7        12      16      20            24                  32
 │  magic        │ V │ op  │ F │  hdr_crc32c  │stream│ pay │  rsv_a  │ pay_crc32c │  rsv_b   │
 │  "BRN0"       │ 1 │ u16 │ 1 │   u32        │ u32  │ u24 │   1 = 0 │   u32       │ 8 = 0    │
```

| Bytes | Field | Type | Notes |
|---|---|---|---|
| 0–3 | `magic` | `[u8; 4]` | Literal `b"BRN0"`. Anything else → `BadMagic`. |
| 4 | `version` | `u8` | Currently `1`. Mismatch → `BadVersion`. |
| 5–6 | `opcode` | `u16` BE | High byte = namespace (`0x00` substrate, `0x01` knowledge). Low byte = op index. |
| 7 | `flags` | `u8` | Bit 7 = `EOS` (end-of-stream). Bit 6 = `MPL` (multi-payload — more frames follow this stream_id). Bit 5 = `CMP` (reserved). Bits 0-4 must be zero. |
| 8–11 | `header_crc32c` | `u32` BE | CRC32C over bytes 0–7 + 12–31. (The CRC field itself is treated as zero during computation.) |
| 12–15 | `stream_id` | `u32` BE | `0` = connection-level (HELLO/WELCOME/AUTH/AUTH_OK/PING/PONG/BYE). Odd = client-allocated. Even = reserved for server. |
| 16–18 | `payload_len` | `u24` BE | Payload length in bytes. Max `2²⁴ − 1`. |
| 19 | `reserved_a` | `u8` | Must be `0`. Non-zero → `ReservedFieldNonZero`. |
| 20–23 | `payload_crc32c` | `u32` BE | CRC32C over the entire payload. `0` when `payload_len == 0`. |
| 24–31 | `reserved_b` | `[u8; 8]` | Must be all zero. |

## Body layout

Immediately after the 32-byte header. Two sub-regions
(spec §03/04 §2):

1. **`rkyv`-encoded structure** — the typed request or response
   body, archived with `rkyv`. Variable length.
2. **Optional raw vector tail** — zero or more `f32` values
   cast via `bytemuck::cast_slice`. Used by `EncodeReq` /
   `EncodeVectorDirectReq` / vector-bearing responses.

The `rkyv` portion ends on an arbitrary byte; 0–3 zero bytes of
padding then align the raw-vector region to a 4-byte boundary.
The receiver:

1. Validates the header CRC.
2. Reads `payload_len` bytes into a 16-byte-aligned buffer
   (rkyv's deref requires this).
3. Validates the payload CRC.
4. Calls `rkyv::check_archived_root::<Body>(buf)` to get a
   typed view (zero-copy — the body is a slice into the buffer).
5. If the body declares a `vector_len`, casts the trailing bytes
   to `&[f32]` via `bytemuck`.

The buffer must outlive the typed view. No copy in the hot path.

## Direction rule

The opcode's low byte determines direction:

- Low byte `< 0x80` → **request** (client → server).
- Low byte `≥ 0x80` → **response** (server → client).

Validated by `Opcode::is_request()` / `Opcode::is_response()`
(`opcode.rs:373-382`). Sending a request opcode in the wrong
direction → `BadOpcode`.

## Multi-payload framing

Logical messages > 16 MiB are split (spec §03/03 §6):

- All but the last frame have `MPL` flag set.
- The last frame has `MPL` clear (and may set `EOS`).
- All frames share `stream_id` and `opcode`.
- Receiver concatenates payloads in arrival order, then decodes.

Multi-payload is rare in practice — vectors are tiny (1.5 KiB
for 384-dim BGE), and text bodies sit well under the cap.

## Streaming

Long-lived responses (`RecallResp`, `PlanResp`, `ReasonResp`,
`QueryResp`, `SubscribeEvent`, …) share one `stream_id` across
multiple frames. Each non-terminal frame omits `EOS`; the final
frame sets it.

Client can cancel via `CancelStream(0x0050)`; server acks with
`CancelStreamAck(0x00D0)`.

## Limits (server-configurable, spec §03/11 §6)

| Limit | Default |
|---|---|
| Max in-flight streams per connection | 1 024 (negotiated in `WELCOME`'s `max_concurrent_streams`) |
| Max active transactions per agent | 16 |
| Max operations per transaction | 1 000 |
| Max transaction wall-time | 60 s |
| Max edges per `ENCODE` | 64 |
| Frames-per-second per connection | 10 000 |

Exceeding these returns one of `StreamLimitExceeded`,
`TransactionLimitExceeded`, `RateLimited`, or
`ConnectionLimitExceeded` (see [`error-codes.md`](error-codes.md)).

## Validation order

The frame is validated bottom-up. On the first failure the
server emits an `Error(0x00FF)` frame, closes the stream, and
(for protocol errors) closes the connection.

1. Magic + version (header.rs:142–149)
2. Reserved fields zero (header.rs:151–152)
3. Flags low bits zero (header.rs:154–155)
4. `payload_len ≤ MAX_PAYLOAD_BYTES` (header.rs:157–162)
5. `header_crc32c` matches recomputed (header.rs:164–165)
6. Stream ID parity (spec §03/11 §2.5)
7. Opcode is a known value (opcode.rs:343)
8. `payload_crc32c` matches recomputed (frame.rs:116–119)
9. `rkyv::check_archived_root` succeeds (`MalformedRkyv` on fail)
10. If applicable, `bytemuck::cast_slice` succeeds with the
    expected length (`MalformedVector`)

## See also

- [`opcodes.md`](opcodes.md) — every opcode with semantics.
- [`error-codes.md`](error-codes.md) — the full error taxonomy.
- [`handshake.md`](handshake.md) — connection establishment.
- [`../../architecture/02-wire-protocol.md`](../../architecture/02-wire-protocol.md) — design rationale.

**Spec:** §03/03, §03/04. **Source:** `crates/brain-protocol/src/header.rs`, `frame.rs`, `lib.rs`.
