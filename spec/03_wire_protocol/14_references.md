# 03.14 References

References specifically for the wire protocol. The full spec-series reference list is in [01.11 References](../01_system_architecture/11_references.md).

## 1. Encoding libraries

- **rkyv** — zero-copy structured serialization for Rust. The encoding for all structured payloads. [GitHub: rkyv/rkyv](https://github.com/rkyv/rkyv).

- **bytemuck** — safe bit-cast operations. The encoding for raw vector payloads. [GitHub: Lokathor/bytemuck](https://github.com/Lokathor/bytemuck).

## 2. Networking and TLS

- **rustls** — the pure-Rust TLS implementation Brain uses. [GitHub: rustls/rustls](https://github.com/rustls/rustls).

- **glommio** — the async runtime. The connection layer is built on Glommio's TCP and TLS abstractions. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio).

- **TLS 1.3** — RFC 8446. [datatracker.ietf.org/doc/html/rfc8446](https://datatracker.ietf.org/doc/html/rfc8446). The minimum TLS version Brain accepts.

## 3. Hashing

- **CRC32C (Castagnoli polynomial)** — used for header and payload checksums. The polynomial is 0x1EDC6F41. SSE 4.2 (`_mm_crc32_u8`, `_mm_crc32_u32`, `_mm_crc32_u64`) and ARMv8.0+ (`crc32cb`, `crc32cw`, `crc32cx`) provide hardware acceleration.

  - **`crc32fast`** — Rust implementation with hardware acceleration. [GitHub: srijs/rust-crc32fast](https://github.com/srijs/rust-crc32fast).

## 4. Identifier formats

- **RFC 9562** — UUID Formats including UUIDv7. [datatracker.ietf.org/doc/rfc9562](https://datatracker.ietf.org/doc/rfc9562/). Used for `AgentId`, `RequestId`, and storage `ShardId`.

  - **`uuid` crate** — Rust library implementing UUIDv7 (and other versions). [GitHub: uuid-rs/uuid](https://github.com/uuid-rs/uuid).

## 5. Wire-protocol design conventions

- **RFC 2119** — Key words for use in RFCs to Indicate Requirement Levels. [datatracker.ietf.org/doc/html/rfc2119](https://datatracker.ietf.org/doc/html/rfc2119). MUST/SHOULD/MAY semantics used throughout this spec.

- **PostgreSQL frontend/backend protocol** — the design of which inspired the per-frame typed structure. [postgresql.org/docs/current/protocol.html](https://www.postgresql.org/docs/current/protocol.html). PostgreSQL's protocol is a good example of a long-lived custom binary protocol.

- **MySQL Client/Server Protocol** — another reference for custom binary protocols. [dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html](https://dev.mysql.com/doc/dev/mysql-server/latest/PAGE_PROTOCOL.html).

- **HTTP/2 framing** — what we considered and rejected. RFC 9113. [datatracker.ietf.org/doc/html/rfc9113](https://datatracker.ietf.org/doc/html/rfc9113). Useful background; we don't follow it.

- **gRPC over HTTP/2** — what we considered and rejected. [grpc.io/docs/what-is-grpc/](https://grpc.io/docs/what-is-grpc/).

## 6. Streaming model background

The stream-multiplexing-over-a-single-connection design is conceptually similar to:

- **HTTP/2 streams** — RFC 9113.
- **QUIC streams** — RFC 9000. [datatracker.ietf.org/doc/html/rfc9000](https://datatracker.ietf.org/doc/html/rfc9000).
- **WebSockets** — RFC 6455. [datatracker.ietf.org/doc/html/rfc6455](https://datatracker.ietf.org/doc/html/rfc6455).

Brain's streams are simpler than any of these (no server-initiated streams in v1, no fancy flow control, no multiplexed push). The simplicity is intentional.

## 7. Error code design

Brain's error codes are inspired by, but not equivalent to:

- **PostgreSQL's SQLSTATE codes** — five-character classification. [postgresql.org/docs/current/errcodes-appendix.html](https://www.postgresql.org/docs/current/errcodes-appendix.html).
- **gRPC status codes** — a small canonical set. [grpc.github.io/grpc/core/md_doc_statuscodes.html](https://grpc.github.io/grpc/core/md_doc_statuscodes.html).
- **HTTP status codes** — RFC 9110.

Brain's set is closer to gRPC's in spirit (small, canonical, semantic) than to PostgreSQL's (granular, hierarchical).

## 8. The handshake design

The HELLO/WELCOME pattern is widespread; specific influences:

- **PostgreSQL's startup message** — the closest analog.
- **MySQL's handshake** — version negotiation, authentication exchange.
- **TLS's ClientHello/ServerHello** — for the version-negotiation pattern.

Brain's handshake is simpler than any of these (single round trip after TLS).

## 9. The opcode catalog inspirations

The opcode set was designed against the cognitive operations rather than imitating any other system. Vocabulary comes from:

- **The Brain data model** — see [02. Data Model](../02_data_model/).
- **Cognitive science conventions** — encode, recall, plan, reason, forget.
- **Database-server traditions** — separate auth, ping/pong liveness, transaction grouping.

## 10. Implementation crates

Crates Brain uses in implementing the wire protocol:

- **glommio** — TCP and TLS via the runtime's I/O primitives. [GitHub: DataDog/glommio](https://github.com/DataDog/glommio).
- **rkyv** — payload serialization. [GitHub: rkyv/rkyv](https://github.com/rkyv/rkyv).
- **bytemuck** — raw vector payload encoding. [GitHub: Lokathor/bytemuck](https://github.com/Lokathor/bytemuck).
- **rustls** — TLS termination. [GitHub: rustls/rustls](https://github.com/rustls/rustls).
- **crc32fast** — header and payload checksums. [GitHub: srijs/rust-crc32fast](https://github.com/srijs/rust-crc32fast).
- **uuid** — identifier formats. [GitHub: uuid-rs/uuid](https://github.com/uuid-rs/uuid).
