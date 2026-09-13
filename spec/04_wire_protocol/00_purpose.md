# 04. Wire Protocol

> **TL;DR.** Brain's binary protocol over TCP. 32-byte fixed frame header, **CBOR-encoded structured payloads**, little-endian `f32` raw vector bytes appended in a trailing section. One unified opcode space: substrate operations at `0x00xx`, typed-graph operations at `0x01xx`. Handshake (HELLO/WELCOME/AUTH/AUTH_OK), streaming for long-running operations, structured error codes, server-side validation of every field. The protocol is the **complete and self-sufficient** contract a third-party client implementer needs to talk to Brain — Brain ships no client library, so this spec stands alone.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of third-party clients and the server's connection layer |
| Voice | Hybrid (rationale + normative MUST/SHOULD) |
| Depends on | [01. System Architecture](../01_architecture/00_purpose.md), [02. Data Model](../02_data_model/00_purpose.md) |
| Referenced by | [05. Operations](../05_operations/00_purpose.md), [06. Client Interface](../06_sdk/00_purpose.md) |

## What this spec defines

The complete wire protocol between Brain clients and Brain servers. The protocol is binary, runs over TCP (optionally TLS-wrapped), and uses a custom framing — not gRPC.

This document specifies everything a third-party client implementer needs to talk to Brain: framing, opcodes (substrate primitives at `0x00xx` plus typed-graph operations at `0x01xx`), payload encodings, handshake, error codes, streaming model, and connection lifecycle.

### Conventions

- **The opcode is `u16`; the `flags` field is `u8`.** Substrate ops live at high byte `0x00` (`ENCODE_REQ = 0x0020`); typed-graph ops live at high byte `0x01` (`ENTITY_CREATE = 0x0130`). See [`03_opcodes.md`](03_opcodes.md) for the full namespace.
- Each typed-graph opcode section in [`08_typed_graph_frames.md`](08_typed_graph_frames.md) and [`09_typed_graph_admin.md`](09_typed_graph_admin.md) has the same structure: request body, response body, error responses, examples / cross-shard notes.
- Every payload is a **CBOR map** ([RFC 8949](https://www.rfc-editor.org/rfc/rfc8949)) validated against the per-opcode field schema in [`05_frame_layouts.md`](05_frame_layouts.md). Senders MUST use a reproducible deterministic encoding (definite-length items, shortest-form integers, fixed per-opcode field order) so payloads are byte-reproducible for the conformance corpus; receivers MUST reject unknown fields and malformed CBOR. (This is reproducibility from a fixed schema, not the full key-sorted RFC 8949 §4.2.1 canonical profile; see [`02_wire_format.md`](02_wire_format.md) §11.2.)
- Optional `WireUuid` fields use the `[0u8; 16]` sentinel rather than a CBOR `null` — UUIDv7's first 48 bits are a timestamp so the all-zero value is unrepresentable as a real id.

## What this document covers

- **Why a custom protocol.** The choice between gRPC, custom binary, REST, and others. ([`01_design.md`](01_design.md))
- **Transport layer.** TCP, optional TLS, default port, connection model, keepalive. ([`01_design.md`](01_design.md))
- **Frame format.** The 32-byte fixed header, payload framing, multi-frame messages. ([`02_wire_format.md`](02_wire_format.md))
- **Payload encoding.** CBOR for structured payloads, a trailing little-endian `f32` section for raw vector bytes, the rationale for splitting them. ([`02_wire_format.md`](02_wire_format.md))
- **Opcodes.** The complete table of client-to-server and server-to-client opcodes. ([`03_opcodes.md`](03_opcodes.md))
- **Handshake.** Connection establishment, version check, authentication. ([`04_handshake.md`](04_handshake.md))
- **Request frames.** Per-opcode frame layouts for everything a client sends. ([`05_frame_layouts.md`](05_frame_layouts.md))
- **Response frames.** Per-opcode frame layouts for everything the server sends back. ([`05_frame_layouts.md`](05_frame_layouts.md))
- **Streaming.** How long-running operations stream incremental results, how stream IDs work, how cancellation works. ([`06_streaming.md`](06_streaming.md))
- **Errors.** Error codes, categories, retry guidance, error frame layouts. ([`07_error_handling.md`](07_error_handling.md))
- **Validation.** What the server validates and rejects. ([`07_error_handling.md`](07_error_handling.md))
- **Versioning.** How protocol versions evolve and how clients negotiate them. ([`03_opcodes.md`](03_opcodes.md))

## What this document does not cover

- **Cognitive operation semantics.** What `RECALL` *means* — defined in [05. Operations](../05_operations/00_purpose.md). This spec defines the bytes; that one defines the meaning.
- **Client ergonomics.** Connection pooling, retry policy, result-streaming helpers — these are the client author's concern; Brain ships no client. See [06. Client Interface](../06_sdk/00_purpose.md).
- **Server internals.** How the server processes a frame after parsing it — that's the connection layer in [01.04 Layers](../01_architecture/04_layers.md) §L1, with downstream layers defined elsewhere.
- **Authentication backends.** Token validation, mTLS certificate pinning, etc. — defined in [17. Observability](../17_observability/00_purpose.md) §Security.

The split between this spec and [05. Operations](../05_operations/00_purpose.md) is sharp: this spec is byte-level; that spec is semantic. A client implementer reads both — and, because Brain ships no SDK, these two specs plus the conformance corpus ([§19](../19_benchmarks/00_purpose.md)) are *everything* a client implementer gets. They are written to be self-sufficient for exactly that reason.

## Audience

The reader is a senior engineer building a client or implementing the server's connection layer. They are comfortable with binary protocol design (have read at least one of: PostgreSQL wire, MongoDB wire, Redis RESP, gRPC frame, AMQP) and with low-level Rust or equivalent.

## Conventions

- **Endianness.** All multi-byte integers in the wire format are big-endian unless stated otherwise. Vectors (raw `f32` bytes) use little-endian (matching common CPU layout).
- **Sizes.** Stated explicitly. The frame header is 32 bytes. Payload sizes are bounded.
- **Bit numbering.** Within a byte, bit 0 is the most significant.
- **Field names.** Match the structure names in the reference implementation where reasonable.

## Position in the spec series

This is spec 04. It depends on:

- [01. System Architecture](../01_architecture/00_purpose.md) — for the layer model and the cognitive primitives.
- [02. Data Model](../02_data_model/00_purpose.md) — for the entities the protocol carries.

It is depended on by:

- [05. Operations](../05_operations/00_purpose.md) — operations are sent over this protocol.
- [06. Client Interface](../06_sdk/00_purpose.md) — which is a stub pointing back here: clients implement this protocol directly.

A reader who hasn't read 01 or 02 will find some terms unfamiliar (MemoryId, AgentId, RequestId). This spec uses them as defined there.

---

*Continue to [`01_design.md`](01_design.md) for why the protocol looks the way it does.*

