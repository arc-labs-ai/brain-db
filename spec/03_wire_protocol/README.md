# 03. Wire Protocol

> **Brain — A Cognitive Substrate for AI Agents**
> Specification document, format version 1.

## Status

| Field | Value |
|---|---|
| Status | Draft |
| Audience | Implementers of clients, SDKs, and the server's connection layer |
| Voice | Hybrid (rationale + normative MUST/SHOULD) |
| Depends on | [01. System Architecture](../01_system_architecture/), [02. Data Model](../02_data_model/) |
| Referenced by | [09. Cognitive Operations](../09_cognitive_operations/), [13. SDK Design](../13_sdk_design/) |

## What this spec defines

The complete wire protocol between Brain clients and Brain servers. The protocol is binary, runs over TCP (optionally TLS-wrapped), and uses a custom framing — not gRPC.

This document specifies everything a third-party client implementer needs to talk to Brain: framing, opcodes, payload encodings, handshake, error codes, streaming model, and connection lifecycle.

## Reading order

| File | Topic |
|---|---|
| [`00_purpose.md`](00_purpose.md) | What this spec covers |
| [`01_design_choices.md`](01_design_choices.md) | Why a custom protocol, not gRPC |
| [`02_transport.md`](02_transport.md) | TCP, TLS, port, connection model |
| [`03_frame_header.md`](03_frame_header.md) | The 32-byte fixed header |
| [`04_payload_encoding.md`](04_payload_encoding.md) | rkyv structured payloads + bytemuck raw vectors |
| [`05_opcodes.md`](05_opcodes.md) | The complete opcode table |
| [`06_handshake.md`](06_handshake.md) | HELLO → WELCOME → AUTH → AUTH_OK |
| [`07_request_frames.md`](07_request_frames.md) | Per-opcode request frame layouts |
| [`08_response_frames.md`](08_response_frames.md) | Per-opcode response frame layouts |
| [`09_streaming.md`](09_streaming.md) | The streaming model and stream IDs |
| [`10_errors.md`](10_errors.md) | Error codes, categories, and propagation |
| [`11_validation.md`](11_validation.md) | Frame and payload validation rules |
| [`12_versioning.md`](12_versioning.md) | Version negotiation and compatibility |
| [`13_open_questions.md`](13_open_questions.md) | Unresolved questions |
| [`14_references.md`](14_references.md) | References |

---

*Continue to [`00_purpose.md`](00_purpose.md) to begin.*
