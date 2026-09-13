# brain-protocol

> Wire protocol (frame format, opcodes, codec) for Brain.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Defines Brain's custom binary wire protocol over TCP. Frames carry a fixed
32-byte header (magic `b"BRN0"`, CRC32C over header and payload, 24-bit/16 MiB
payload cap) wrapping a CBOR payload. The crate owns frame/header/opcode codec
plumbing, the `RequestBody`/`ResponseBody` dispatch envelopes, the connection
handshake and stream-control ops, the per-domain operation request/response
types (memory, entity, statement, relation, query, procedural, txn, subscribe,
admin, extractor), the structured error taxonomy, and the schema DSL surface
(AST, pest-based parser, validator, and schema-management wire ops). Pure
encode/decode — `#![forbid(unsafe_code)]`.

## Key modules

| Module | Purpose |
|---|---|
| `codec` | Bytes-on-the-wire plumbing: `Header`, `Frame`, `Opcode`, CRC, CBOR. Knows nothing of op semantics. |
| `envelope` | `RequestBody` / `ResponseBody` dispatch enums, `ErrorResponse`, core ↔ wire conversions. |
| `connection` | Handshake (`negotiate`, HELLO/WELCOME/AUTH) and stream control (PING/PONG/CANCEL_STREAM/BYE). |
| `ops` | Per-domain wire ops — one file per capability, holding request/response/view types together. |
| `schema` | Schema DSL AST, parser, validator, plus the schema-management wire ops. |
| `shared` | Wire primitives and enums shared across op families. |
| `error` | `ErrorCategory` / `ErrorCode` / `ProtocolError` taxonomy. |

## Where it fits

Depends on `brain-core` plus encoding/parsing leaves (`ciborium`, `crc32c`,
`bytemuck`, `serde`, `pest`). It is the contract between clients and the server;
consumed by `brain-metadata`, `brain-ops`, and `brain-server`.

## Spec

- Wire protocol: [`../../spec/04_wire_protocol/02_wire_format.md`](../../spec/04_wire_protocol/02_wire_format.md)
- Opcodes: [`../../spec/04_wire_protocol/03_opcodes.md`](../../spec/04_wire_protocol/03_opcodes.md)
- Handshake: [`../../spec/04_wire_protocol/04_handshake.md`](../../spec/04_wire_protocol/04_handshake.md)
- Error handling: [`../../spec/04_wire_protocol/07_error_handling.md`](../../spec/04_wire_protocol/07_error_handling.md)
- Schema DSL: [`../../spec/03_schema/01_grammar.md`](../../spec/03_schema/01_grammar.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
