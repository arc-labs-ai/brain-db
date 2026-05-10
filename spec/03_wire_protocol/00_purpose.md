# 03.00 Purpose

This spec defines the wire protocol between Brain clients and Brain servers — the bytes that flow over TCP. Sufficient for a third-party implementer to build a Brain-compatible client or server from scratch.

## What this document covers

- **Why a custom protocol.** The choice between gRPC, custom binary, REST, and others. ([`01_design_choices.md`](01_design_choices.md))
- **Transport layer.** TCP, optional TLS, default port, connection model, keepalive. ([`02_transport.md`](02_transport.md))
- **Frame format.** The 32-byte fixed header, payload framing, multi-frame messages. ([`03_frame_header.md`](03_frame_header.md))
- **Payload encoding.** rkyv for structured payloads, bytemuck for raw vector bytes, the rationale for splitting them. ([`04_payload_encoding.md`](04_payload_encoding.md))
- **Opcodes.** The complete table of client-to-server and server-to-client opcodes. ([`05_opcodes.md`](05_opcodes.md))
- **Handshake.** Connection establishment, version negotiation, authentication. ([`06_handshake.md`](06_handshake.md))
- **Request frames.** Per-opcode frame layouts for everything a client sends. ([`07_request_frames.md`](07_request_frames.md))
- **Response frames.** Per-opcode frame layouts for everything the server sends back. ([`08_response_frames.md`](08_response_frames.md))
- **Streaming.** How long-running operations stream incremental results, how stream IDs work, how cancellation works. ([`09_streaming.md`](09_streaming.md))
- **Errors.** Error codes, categories, retry guidance, error frame layouts. ([`10_errors.md`](10_errors.md))
- **Validation.** What the server validates and rejects. ([`11_validation.md`](11_validation.md))
- **Versioning.** How protocol versions evolve and how clients negotiate them. ([`12_versioning.md`](12_versioning.md))

## What this document does not cover

- **Cognitive operation semantics.** What `RECALL` *means* — defined in [09. Cognitive Operations](../09_cognitive_operations/). This spec defines the bytes; that one defines the meaning.
- **SDK ergonomics.** How a Python or Rust client wraps the protocol — defined in [13. SDK Design](../13_sdk_design/).
- **Server internals.** How the server processes a frame after parsing it — that's the connection layer in [01.04 Layers](../01_system_architecture/04_layers.md) §L1, with downstream layers defined elsewhere.
- **Authentication backends.** Token validation, mTLS certificate pinning, etc. — defined in [14. Observability + Operations](../14_observability_ops/) §Security.

The split between this spec and [09. Cognitive Operations](../09_cognitive_operations/) is sharp: this spec is byte-level; that spec is semantic. A client implementer reads both.

## Audience

The reader is a senior engineer building a client or implementing the server's connection layer. They are comfortable with binary protocol design (have read at least one of: PostgreSQL wire, MongoDB wire, Redis RESP, gRPC frame, AMQP) and with low-level Rust or equivalent.

## Conventions

- **Endianness.** All multi-byte integers in the wire format are big-endian unless stated otherwise. Vectors (raw `f32` bytes) use little-endian (matching common CPU layout).
- **Sizes.** Stated explicitly. The frame header is 32 bytes. Payload sizes are bounded.
- **Bit numbering.** Within a byte, bit 0 is the most significant.
- **Field names.** Match the structure names in the reference implementation where reasonable.

## Position in the spec series

This is spec 03. It depends on:

- [01. System Architecture](../01_system_architecture/) — for the layer model and the cognitive primitives.
- [02. Data Model](../02_data_model/) — for the entities the protocol carries.

It is depended on by:

- [09. Cognitive Operations](../09_cognitive_operations/) — operations are sent over this protocol.
- [13. SDK Design](../13_sdk_design/) — SDKs implement this protocol.

A reader who hasn't read 01 or 02 will find some terms unfamiliar (MemoryId, AgentId, RequestId). This spec uses them as defined there.

---

*Continue to [`01_design_choices.md`](01_design_choices.md) for why the protocol looks the way it does.*
