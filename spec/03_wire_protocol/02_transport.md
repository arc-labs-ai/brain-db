# 03.02 Transport Layer

The transport for Brain's wire protocol is TCP, optionally wrapped in TLS.

## 1. TCP

### 1.1 Default port

The IANA-assigned port for Brain in v1 is **`7474`**. (Subject to formal IANA assignment; this is the documented default.)

Operators MAY run Brain on a different port. Clients accept a server-supplied address and port from configuration.

### 1.2 TCP options

The server SHOULD set the following TCP options on accepted connections:

- `TCP_NODELAY` — disable Nagle's algorithm. Brain's frames are typically small and latency-sensitive; Nagle's batching adds milliseconds of latency for no benefit.
- `SO_KEEPALIVE` — enable TCP keepalive at the OS level. Recommended server defaults: **idle 75 s, interval 15 s, retries 9** (~210 s detection budget). This catches dead clients without application-level pings; the longer budget reflects that a single server tolerates many concurrent clients and shouldn't probe aggressively across all of them.
- `SO_REUSEADDR` (server only) — for graceful restart, allowing the server to rebind the listening socket.

Clients SHOULD set:

- `TCP_NODELAY` — same reason.
- `SO_KEEPALIVE` — to detect server crashes that don't close the connection cleanly. Recommended SDK defaults: **idle 30 s, interval 10 s, retries 3** (~60 s detection budget). Aggressive vs the server side because a client typically tracks one server, so faster probing is cheap; and operators want their next op to fail fast (and trigger transparent reconnect via §13/04 retries) rather than stall on a dead route. On platforms that don't expose the retries socket option (macOS, Windows), idle + interval still apply and the OS default retry count provides a slightly looser bound (~80 s).

### 1.3 Connection model

A single TCP connection can carry many concurrent operations, identified by stream IDs (see [`09_streaming.md`](09_streaming.md)). Clients SHOULD reuse connections rather than creating one per operation.

The server limits connections per agent (default: 100) and per-IP (default: 1000) to prevent abuse. Limits are configurable.

### 1.4 Connection lifecycle

```
Client                                    Server
  │                                         │
  │  TCP connect ────────────────────────►  │
  │  ◄──────────────────────── TCP accept   │
  │                                         │
  │  (optional: TLS handshake)              │
  │  TLS ClientHello ─────────────────────► │
  │  ◄──────────────── TLS ServerHello..    │
  │                                         │
  │  HELLO frame ──────────────────────────►│
  │  ◄────────────────────── WELCOME frame  │
  │                                         │
  │  AUTH frame ───────────────────────────►│
  │  ◄────────────────────── AUTH_OK frame  │
  │                                         │
  │     (now established; operations flow)  │
  │  ENCODE / RECALL / ... ───────────────► │
  │  ◄──────────────────────── ACK / data   │
  │                                         │
  │  ...                                    │
  │                                         │
  │  BYE ──────────────────────────────────►│
  │  ◄──────────────────────────────── BYE  │
  │  TCP close                              │
```

Detailed handshake flow in [`06_handshake.md`](06_handshake.md).

## 2. TLS

### 2.1 When to use TLS

TLS SHOULD be used whenever the connection traverses an untrusted network (Internet-facing deployments, multi-tenant infrastructure, etc.).

For internal-only deployments on a private network with no untrusted access, TLS is OPTIONAL. Operators may run Brain without TLS to save the handshake cost; they MUST be aware they're trading off confidentiality and integrity for ~1 ms of first-connection latency.

### 2.2 TLS version

Only **TLS 1.3** is supported. TLS 1.2 and earlier are refused.

This is a deliberate constraint: TLS 1.3 has clean security properties, simpler handshakes (1-RTT or 0-RTT), and fewer footguns. Limiting to 1.3 simplifies our TLS configuration story significantly.

### 2.3 Cipher suites

TLS 1.3's mandatory cipher suites (per [RFC 8446](https://datatracker.ietf.org/doc/html/rfc8446)) are:

- `TLS_AES_128_GCM_SHA256`
- `TLS_AES_256_GCM_SHA384`
- `TLS_CHACHA20_POLY1305_SHA256`

The server MUST support at least the first two and the client MUST support at least one of them.

### 2.4 Certificate validation

By default, the client validates the server's certificate against the system trust store. Operators may configure:

- A custom trust anchor (for self-signed deployments).
- mTLS — both client and server present certificates.

Hostname verification SHOULD use the standard SAN (Subject Alternative Name) match per [RFC 6125](https://datatracker.ietf.org/doc/html/rfc6125).

### 2.5 SNI

Clients SHOULD send Server Name Indication (SNI) on connect. Servers may use SNI to route to multiple Brain instances behind a single TCP endpoint, though this is not the typical deployment.

### 2.6 ALPN

ALPN SHOULD use the protocol identifier `"brain/1"` for protocol version 1. This lets a TLS-terminating proxy distinguish Brain traffic from other protocols on the same port.

## 3. Connection establishment

### 3.1 Handshake budget

The full connect-and-handshake budget:

| Step | Typical | Worst case |
|---|---|---|
| TCP connect (3-way) | 0.5–1 ms (LAN) | varies |
| TLS handshake (1-RTT) | 1–2 ms (LAN, TLS 1.3) | 3–10 ms |
| HELLO/WELCOME | 0.1–0.5 ms | 1–2 ms |
| AUTH/AUTH_OK | 0.1–1 ms (token); 5–20 ms (mTLS verification, depends on cert chain) | varies |
| **Total to first operation** | **~3–5 ms (LAN, TLS, token auth)** | **~15–30 ms** |

The total is amortized across many subsequent operations on the same connection. A connection serving thousands of operations pays the handshake cost once.

### 3.2 Connection reuse

Clients SHOULD maintain a connection pool. The recommended SDK behavior:

- Pool size per server: 4–16 connections (configurable).
- Connections kept alive indefinitely; recycled on errors or after a max-idle time.
- Operations distributed across pool entries (round-robin or least-busy).

Per-operation connection creation is wasteful and not the recommended pattern.

## 4. Backpressure

The protocol uses TCP-level flow control for backpressure:

- When the server can't keep up, its receive buffer fills. The TCP window narrows. The client's writes block.
- When the client can't read fast enough, its receive buffer fills. The server's writes block.

There is no application-level flow control beyond this. Stream-level cancellation exists (see [`09_streaming.md`](09_streaming.md) §6) but doesn't slow the producer; it just stops the stream.

This is intentional. TCP flow control is well-understood and reliable; layering an application-level scheme on top adds complexity for little benefit.

## 5. Concurrency

### 5.1 Multiple streams per connection

Many operations can be in flight on one connection simultaneously. Each operation is a stream, identified by stream_id (see [`09_streaming.md`](09_streaming.md)).

The server processes streams concurrently within its per-shard concurrency limits. There's no per-connection sequencer that serializes them.

### 5.2 Frame interleaving

Frames from different streams may be interleaved on the wire. The reader demultiplexes by stream_id.

Frames within a single stream are sequential — the server emits them in order, and the client may rely on that order.

### 5.3 Out-of-order at the connection level

Within a single TCP connection, frames are ordered (TCP guarantees this). Out-of-order observation only happens across stream boundaries — stream A's frame N may be observed after stream B's frame M, even if B was started later. That's the point of streams: they're independent.

## 6. Idle behavior

### 6.1 Server idle timeout

If a connection is idle (no frames in or out) for more than the configured idle timeout (default: 5 minutes), the server SHOULD send a `PING` frame. If the client doesn't respond within the configured ping timeout (default: 30 seconds), the server closes the connection.

### 6.2 Client idle behavior

A client that's idle but wants to keep the connection alive SHOULD send periodic `PING` frames (default cadence: every 30 seconds). The server replies with `PONG`.

This is application-level keepalive, separate from TCP keepalive. TCP keepalive catches dead connections; application keepalive catches dead servers.

## 7. Graceful close

### 7.1 BYE frames

Either side can initiate close by sending a `BYE` frame. The recipient sends its own `BYE` and closes the TCP connection.

A `BYE` indicates "I'm done; finish what's in flight, then close". In-flight streams complete normally; no new streams may be initiated.

### 7.2 Abrupt close

A side that needs to close immediately just closes the TCP connection. The peer sees a connection error; in-flight operations fail with `ConnectionLost`.

This is the only path on emergency shutdown or panic conditions; otherwise, the graceful BYE flow is preferred.

## 8. Reconnection

### 8.1 Automatic reconnection

Clients SHOULD reconnect automatically on connection loss, with reasonable backoff (exponential, capped at ~30 seconds).

Reconnection re-establishes the connection from scratch: TCP, TLS, handshake. Cached connection state (session_id, agent context) is lost; the client must recreate it.

### 8.2 Stream resumption

In-flight streams cannot be resumed across reconnection. The exception is `SUBSCRIBE`, which carries a `from_lsn` parameter to resume the subscription from a specific log position. See [09. Cognitive Operations](../09_cognitive_operations/) §SUBSCRIBE.

For other operations (`RECALL`, `PLAN`, etc.), the client retries from scratch with idempotency (where applicable via `request_id`) on the new connection.

## 9. Network constraints

### 9.1 MTU and fragmentation

Brain frames are typically under 1500 bytes (one Ethernet MTU); large `RECALL` results may exceed this. TCP handles segmentation; we don't worry about MTU at the application level.

For very large frames (>16 MiB), the protocol restricts payload size (see [`03_frame_header.md`](03_frame_header.md) §3.4); larger transfers must use streaming.

### 9.2 Latency tolerance

The protocol assumes sub-millisecond network latency to typical clients. WAN latency is supported but not optimized for; cross-region calls have correspondingly higher latency floors.

### 9.3 Bandwidth

Brain's typical workload is moderately bandwidth-intensive. See [01.05 Hardware](../01_system_architecture/05_hardware.md) §5.2 for the bandwidth analysis.

## 10. IPv4 and IPv6

The server SHOULD listen on both IPv4 and IPv6 by default. Client SDKs SHOULD prefer IPv6 when both are available (consistent with [RFC 6724](https://datatracker.ietf.org/doc/html/rfc6724) destination address selection).

---

*Continue to [`03_frame_header.md`](03_frame_header.md) for the frame format.*
