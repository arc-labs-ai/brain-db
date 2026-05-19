# Wire protocol reference

**Audience:** anyone implementing an SDK, debugging a connection,
or auditing the protocol surface.

**Goal:** *exact, look-it-up information*. Field widths, opcode
values, error codes. Not "why this design" (see
[`../../architecture/02-wire-protocol.md`](../../architecture/02-wire-protocol.md));
not "how do I send a request" (see [`../../guides/sdk/`](../../guides/sdk/)).

## Pages

| Page | Covers |
|---|---|
| [`frame-format.md`](frame-format.md) | Frame header layout, body framing, length limits |
| [`opcodes.md`](opcodes.md) | Every opcode (substrate + knowledge), request/response body shapes |
| [`error-codes.md`](error-codes.md) | The stable error taxonomy and what each code means |
| [`handshake.md`](handshake.md) | HELLO, capability negotiation, version compatibility |

## Encoding

- **Wire codec:** `rkyv` archived structs + `bytemuck` casts. The
  request body is the rkyv-archived form of the request struct;
  the response body is the rkyv-archived form of the response
  struct.
- **Endianness:** little-endian throughout (rkyv default).
- **Alignment:** rkyv requires 16-byte alignment of the archived
  region. Frame parsers must align before deref.
- **Zero-copy:** responses returned to callers are slices into
  the receive buffer. The buffer must outlive the response.

## See also

- [`../../architecture/02-wire-protocol.md`](../../architecture/02-wire-protocol.md)
  — design rationale, zero-copy story, why rkyv.
- [`../../../spec/03_wire_protocol/`](../../../spec/03_wire_protocol/)
  — authoritative spec (14 sub-files).
