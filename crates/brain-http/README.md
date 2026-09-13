# brain-http

> HTTP/1.1, WebSocket, and SSE transport for the Brain substrate.

Internal workspace crate of **[Brain](../../README.md)** — a memory database for
AI agents. Not published to crates.io; consumed by other `brain-*` crates and
ultimately `brain-server`. Apache-2.0.

## What it does

Provides the HTTP/1.1, WebSocket, and Server-Sent Events transport used by
Brain's operator admin listener. It is built on `hyper` 1.x and is
HTTP-version-neutral by construction — hyper owns wire parsing, keep-alive,
chunked encoding, body backpressure, and combinators, while this crate owns the
pieces above the wire: a small `match`-based router, the Brain `Error` taxonomy,
correct SSE flush-after-every-event discipline, and an explicit WebSocket close
handshake. The transport is feature-gated so the server, client, WebSocket, SSE,
and TLS surfaces compile only when needed.

## Key modules

- `service` — `AsyncHandler`, `service_fn` (the surface every handler uses).
- `router` — small `match`-based request router (feature `server`).
- `server` / `tcp` — accept loop, connection handling, graceful shutdown.
- `sse` — Server-Sent Events helpers with per-event flushing.
- `ws` — WebSocket upgrade + close handshake via `tokio-tungstenite`.
- `client` — async HTTP client (feature `client`; not wired in-workspace).
- `body` — streaming body types and combinators.
- `error` — the `Error` / `Result` taxonomy.
- `observability` — tracing integration for the transport.

## Where it fits

Depends on `brain-core` and the `hyper` stack (`hyper`, `http`, `http-body`,
`bytes`), with `tokio` behind the `server`/`client` features. Consumed by
`brain-server`, which builds the admin HTTP listener on top of it.

## Spec

- [`../../spec/17_observability/04_admin_ops.md`](../../spec/17_observability/04_admin_ops.md)
- [`../../spec/04_wire_protocol/06_streaming.md`](../../spec/04_wire_protocol/06_streaming.md)

## License

Apache-2.0 — see [`../../LICENSE`](../../LICENSE).
