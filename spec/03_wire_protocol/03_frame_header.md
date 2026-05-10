# 03.03 Frame Header

Every frame on the wire begins with a fixed 32-byte header. This file specifies its layout.

## 1. The 32-byte header

```
 0                   1                   2                   3
 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
|              magic = "BRN0" (4 bytes)                         |
+---------------+---------------+-------------------------------+
|   version (8) |   opcode (8)  |          flags (16)           |
+---------------+---------------+-------------------------------+
|                  header_crc32c (32)                           |
+---------------------------------------------------------------+
|                     stream_id (32)                            |
+---------------------------------------------------------------+
|   payload_len (24, big-endian)                |   reserved(8) |
+-----------------------------------------------+---------------+
|                  payload_crc32c (32)                          |
+---------------------------------------------------------------+
|                       reserved (32)                           |
+---------------------------------------------------------------+
|                       reserved (32)                           |
+---------------------------------------------------------------+
```

Total: 32 bytes.

Field-by-field:

| Bytes | Field | Type | Purpose |
|---|---|---|---|
| 0–3 | `magic` | 4 ASCII chars | Identifies a Brain frame: `"BRN0"` (0x42 0x52 0x4E 0x30) |
| 4 | `version` | `u8` | Protocol version. Initially 1; bumps on incompatible changes. |
| 5 | `opcode` | `u8` | The operation type. See [`05_opcodes.md`](05_opcodes.md). |
| 6–7 | `flags` | `u16` | Frame-level flags (see §2). |
| 8–11 | `header_crc32c` | `u32` | CRC32C of bytes 0–7 plus bytes 12–31 (i.e., the rest of the header excluding this field, treated as zero during computation). |
| 12–15 | `stream_id` | `u32` | The stream this frame belongs to (see [`09_streaming.md`](09_streaming.md)). |
| 16–18 | `payload_len` | `u24` (24-bit big-endian) | Payload length in bytes; max 16,777,215 (16 MiB - 1). |
| 19 | reserved | `u8` | Must be zero; reserved for future expansion. |
| 20–23 | `payload_crc32c` | `u32` | CRC32C of the payload (zero if `payload_len = 0`). |
| 24–31 | reserved | 8 bytes | Reserved for future expansion. Must be zero. |

All multi-byte integers are **big-endian**.

## 2. Flags

The 16-bit `flags` field encodes per-frame metadata:

```
bit 15  14  13  12  11  10  9   8   7   6   5   4   3   2   1   0
   +---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+
   |EOS|MPL|CMP|...                                              ...|
   +---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+---+
```

| Bit | Name | Meaning |
|---|---|---|
| 15 | `EOS` | End of stream — last frame of this stream. |
| 14 | `MPL` | Multi-payload — payload spans multiple frames; concatenate to reconstruct. |
| 13 | `CMP` | Compressed — payload is zstd-compressed. (Reserved; not used in v1.) |
| 12-0 | reserved | Must be zero. |

The flags `EOS` and `MPL` are mutually compatible: a multi-payload final frame has both. A single-frame final response has only `EOS`.

## 3. Field details

### 3.1 magic

The 4-byte sequence `0x42 0x52 0x4E 0x30` (ASCII `"BRN0"`).

The `0` in the trailing position is a generation marker. If we ever need a fundamentally incompatible new framing, we'd use `"BRN1"`. Within `"BRN0"`-framed protocols, the `version` field handles compatible evolution.

A reader that sees a different magic on the first frame of a connection MUST close the connection — this isn't a Brain frame.

### 3.2 version

The protocol version. Currently 1.

The version is checked at handshake time (the `HELLO` frame's negotiation). Once negotiated, all subsequent frames on the connection MUST have the same version. A frame with a different version is a protocol error and the connection is closed.

### 3.3 opcode

The operation type. See [`05_opcodes.md`](05_opcodes.md) for the full table.

Opcodes 0x00–0x7F are server-bound (client → server); 0x80–0xFF are client-bound (server → client).

### 3.4 payload_len

The length of the payload in bytes, as a 24-bit big-endian unsigned integer. Maximum: 16,777,215 (just under 16 MiB).

A frame with `payload_len = 0` has no payload bytes after the header. Both `EOS`-only frames and pure ACK frames typically have empty payloads.

For payloads larger than 16 MiB, use multi-payload framing: split the payload across multiple frames, all but the last having `MPL` set, and concatenate at the receiver.

### 3.5 stream_id

The stream this frame belongs to. See [`09_streaming.md`](09_streaming.md) for the streaming model.

`stream_id = 0` is reserved for connection-level frames (PING, PONG, BYE, HELLO, WELCOME, AUTH, AUTH_OK, error frames not associated with a stream).

`stream_id` 1, 3, 5, ... (odd) are client-allocated. `stream_id` 2, 4, 6, ... (even) are reserved for server-initiated streams in the future; not used in v1.

### 3.6 header_crc32c

CRC32C of the header. Computed over bytes 0–7 followed by bytes 12–31 — i.e., the entire header minus the `header_crc32c` field itself. During computation, the `header_crc32c` field is treated as if zero (or omitted).

The polynomial is the [Castagnoli polynomial 0x1EDC6F41](https://en.wikipedia.org/wiki/Cyclic_redundancy_check). Hardware acceleration is available on x86 (SSE 4.2) and ARM (CRC32 extension); see [01.05 Hardware](../01_system_architecture/05_hardware.md) §2.1.

A frame with mismatched `header_crc32c` is treated as corruption: the receiver MUST close the connection with a `BadFrame` error.

### 3.7 payload_crc32c

CRC32C of the payload. Computed over the payload bytes (after the 32-byte header).

If `payload_len = 0`, `payload_crc32c` MUST also be zero.

A frame with mismatched `payload_crc32c` is corruption: connection close with `BadPayload` error.

### 3.8 Reserved fields

The 8 bytes at positions 24–31 and the single byte at position 19 are reserved for future use. They MUST be zero in v1. Receivers MUST verify they are zero; non-zero values are protocol errors.

The reserved space provides room for future additions:

- More flags.
- Per-frame priority indicators.
- Tracing IDs (alternatives to OpenTelemetry's W3C trace context).

The exact use is intentionally open; we want flexibility within the existing framing.

## 4. Frame parsing

### 4.1 The reader's algorithm

```
loop:
    read 32 bytes (header)
    verify magic == "BRN0"
    verify version matches negotiated version
    verify reserved fields are zero
    verify header_crc32c
    if payload_len > 0:
        read payload_len bytes
        verify payload_crc32c
    dispatch by opcode and stream_id
```

The reader MUST NOT trust any field until the header CRC is verified. Out-of-bounds payload_len, garbage opcodes, etc., are all caught by the header CRC check (assuming the CRC was set correctly by the sender; if the CRC matches but the field is invalid, it's a sender bug, not corruption).

### 4.2 Why two CRCs

Two checksums seem redundant but serve different purposes:

- **`header_crc32c`** validates the header, so the reader can trust `payload_len` and `payload_crc32c` enough to read the payload.
- **`payload_crc32c`** validates the payload, catching corruption that occurred after the header was written.

A single CRC over both header and payload would require buffering the entire payload before the reader could trust it. Two CRCs let the reader stream-process: parse header, allocate buffer, read payload, validate.

### 4.3 Why CRC32C, again

Already justified in [`01_design_choices.md`](01_design_choices.md) §7. CRC32C is fast, hardware-accelerated, adequate for transmission-error detection. TLS handles adversarial concerns; CRCs handle accidental corruption.

## 5. Frame size

Minimum frame size: 32 bytes (header only, no payload). Maximum frame size: 32 + 16,777,215 = 16,777,247 bytes.

The 16 MiB limit on payload prevents pathological frames that would block the connection while being read. For larger transfers, multi-payload framing (the `MPL` flag) is used.

## 6. Multi-payload frames

When a logical message exceeds 16 MiB, the sender splits it into multiple frames:

- All frames have the same `stream_id` and `opcode`.
- All but the last have `MPL = 1`.
- The last frame has `MPL = 0` (and may have `EOS = 1` if it's the end of the stream).
- The receiver concatenates the payloads in receive order.

This is rarely needed in practice. Most operations produce small frames; multi-payload kicks in only for very large `RECALL` results (10000+ memories) or large bulk transfers.

## 7. Frame examples

### 7.1 PING frame

```
Field            Value
magic            "BRN0"
version          1
opcode           0x10 (PING)
flags            0x0000
header_crc32c    <computed>
stream_id        0
payload_len      0
reserved         0
payload_crc32c   0
reserved         0..0
```

32 bytes total. No payload.

### 7.2 RECALL request frame

```
Field            Value
magic            "BRN0"
version          1
opcode           0x21 (RECALL_REQ)
flags            0x0000
header_crc32c    <computed>
stream_id        7 (client-allocated, odd)
payload_len      <size of rkyv-encoded RecallRequest>
reserved         0
payload_crc32c   <computed>
reserved         0..0
```

Plus the rkyv-encoded RecallRequest payload. See [`07_request_frames.md`](07_request_frames.md) for layout.

### 7.3 RECALL response frame (intermediate, with one result)

```
Field            Value
magic            "BRN0"
version          1
opcode           0xA1 (RECALL_RESP)
flags            0x0000  (not EOS yet)
header_crc32c    <computed>
stream_id        7
payload_len      <size of one MemoryResult>
reserved         0
payload_crc32c   <computed>
reserved         0..0
```

### 7.4 RECALL response frame (final, EOS)

```
Field            Value
magic            "BRN0"
version          1
opcode           0xA1 (RECALL_RESP)
flags            0x8000  (EOS)
header_crc32c    <computed>
stream_id        7
payload_len      0  (or final batch of results)
reserved         0
payload_crc32c   <computed>
reserved         0..0
```

## 8. Endianness summary

| Field | Endianness |
|---|---|
| `magic` | byte order (literal ASCII) |
| `version` | single byte |
| `opcode` | single byte |
| `flags` | big-endian `u16` |
| `header_crc32c` | big-endian `u32` |
| `stream_id` | big-endian `u32` |
| `payload_len` | big-endian `u24` |
| reserved bytes | byte order; must be zero |
| `payload_crc32c` | big-endian `u32` |

Within payloads, encodings (rkyv structures and bytemuck-cast vectors) have their own conventions; see [`04_payload_encoding.md`](04_payload_encoding.md).

---

*Continue to [`04_payload_encoding.md`](04_payload_encoding.md) for payload encoding.*
