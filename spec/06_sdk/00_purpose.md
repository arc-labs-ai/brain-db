# 06. Client Interface

> **TL;DR.** Brain ships no first-party SDK or client library. Clients talk to Brain directly over the wire protocol ([§04](../04_wire_protocol/00_purpose.md)) — a binary framing over TCP with self-describing CBOR payloads and documented per-opcode field schemas. Any language can speak it with a stock CBOR library; none of it requires a Brain-provided client. This section is intentionally a stub.

## Status

| Field | Value |
|---|---|
| Status | Stub (standalone-database posture) |
| Audience | Third-party client implementers |
| Depends on | [04. Wire Protocol](../04_wire_protocol/00_purpose.md) |
| Referenced by | — |

## Brain ships no first-party client

Brain is a standalone database. Its public interface is the wire protocol defined in [§04](../04_wire_protocol/00_purpose.md): a portable binary framing over TCP, self-describing CBOR payloads, and a documented field schema per opcode. Any program in any language can speak it using a stock CBOR library — no Brain-provided SDK is required, and none ships.

What this means:

- **No SDK.** Brain does not publish Python / TypeScript / Go / Rust client libraries. Building one — for any language — is out of scope for the Brain project and left to third parties or future work.
- **The protocol is the contract.** Everything a client needs is in §04: framing, opcodes, handshake, payload schemas, error codes, streaming. A conformance-vector corpus (see [§19](../19_benchmarks/00_purpose.md)) lets a client author verify an implementation byte-for-byte against the reference server.
- **Admin is plain HTTP.** Operator actions (snapshots, stats, worker control) are served on the admin HTTP listener and are reachable with `curl` or any HTTP tool — no CLI ships.
- **Hello world is raw protocol.** The byte exchange for a minimal `HELLO → ENCODE → RECALL` session is shown in [§04](../04_wire_protocol/00_purpose.md), not here.

## Where the SDK-shaped concerns live now

The client-side concerns a thick SDK would own — connection pooling, retry/backoff policy, idempotency-key generation, result-streaming ergonomics — are the client author's responsibility, not Brain's. Their **server-side** counterparts are specified where they belong:

- Connection lifecycle, keepalive, TLS — [§04.01 Design and Transport](../04_wire_protocol/01_design.md).
- Which errors are retryable, and how the server signals back-off — [§04.07 Error Handling](../04_wire_protocol/07_error_handling.md) and [§05 Operations](../05_operations/00_purpose.md).
- Idempotency (per-agent `RequestId`, 24 h TTL) — [§05 Operations](../05_operations/00_purpose.md) and [§10 Metadata](../10_metadata/00_purpose.md).
- Streaming framing (per-frame EOS, stream IDs, cancellation) — [§04.06 Streaming](../04_wire_protocol/06_streaming.md).

There is nothing else in this section. A client author reads §04 (the bytes) and §05 (the meaning).
