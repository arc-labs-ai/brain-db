# 13.03 Connection Management

How the SDK manages TCP connections to the substrate.

## 1. The connection pool

The Client maintains a pool of connections per server:

```
Client
  └── ServerConnections
        ├── Server: host1:9090
        │   ├── Connection 1 (idle)
        │   ├── Connection 2 (in use)
        │   └── Connection 3 (in use)
        └── Server: host2:9090
            ├── Connection 1 (idle)
            └── Connection 2 (idle)
```

Connections are reused across requests. The pool size is configurable:

- Default min: 1 per server.
- Default max: 8 per server.

## 2. Per-connection multiplexing

Each connection can carry multiple in-flight requests via stream IDs (per [03.07 Streaming](../03_wire_protocol/07_multiplexing.md)).

So connection count × stream count = total concurrent requests possible.

For a default client (8 connections × 1024 streams = 8192 concurrent requests per server). More than most agents will ever use.

## 3. Connection establishment

When the SDK needs a new connection:

```
1. TCP connect to the server.
2. TLS handshake (if configured).
3. Brain protocol handshake:
   a. Send version + supported features.
   b. Receive server's version + features.
   c. Negotiate.
4. Authenticate:
   a. Send auth credentials.
   b. Receive auth ack or error.
5. Connection is ready.
```

All this happens at first use; subsequent requests reuse the connection.

## 4. The "first request slow" effect

The first request on a fresh connection is slower (connect + handshake + auth). Subsequent requests are fast (just protocol).

For applications wanting low first-request latency, pre-warming:

```rust
client.warm_up().await?;    // Pre-establish min connections
```

This establishes connections eagerly so the first real request is fast.

## 5. Idle connection management

Idle connections:

- Receive periodic keep-alive frames (default every 30 sec).
- Are closed if idle for too long (default 5 min).
- Are validated before reuse.

Validation checks:

- TCP socket is still open.
- Last keep-alive ack was recent.

## 6. Reconnection

If a connection drops:

```
1. The SDK detects via I/O error or keep-alive timeout.
2. Outstanding requests on that connection fail with NetworkError.
3. The connection is removed from the pool.
4. On the next request, a new connection is established.
```

Outstanding requests are NOT auto-retried at this layer (see [13.04 Retries](04_retries.md)).

## 7. Server failover

For multi-server clients:

- Try the first server.
- If unreachable, try the next.
- Cycle through; if all fail, error.

The SDK can optionally use **client-side load balancing**:

- Round-robin: distribute requests across servers.
- Weighted: based on server capacity.
- Sticky-by-key: consistent assignment.

For a sharded substrate (v1 single-node sharding), the SDK uses the routing table to send each request to the right shard's server.

## 8. The shard-aware routing

In clustered mode (v2), the SDK has the cluster's routing table:

- Each shard's home node.
- Authentication state per node.

```
client.encode(...)
  → SDK consults routing table
  → SDK picks the right node
  → SDK sends frame on a connection to that node
```

If routing is stale (`WrongShard` error), the SDK refreshes and retries.

## 9. The "bootstrap" pattern

The Client is initialized with bootstrap addresses:

```
client = Client::new(["host1:9090", "host2:9090"])
```

These addresses are tried first. The SDK can be configured to learn the full membership from the cluster:

```rust
let client = Client::builder()
    .bootstrap(["host1:9090"])
    .discovery(Discovery::ClusterMember)    // Auto-learn other nodes
    .build();
```

Auto-discovery happens at startup and periodically (e.g., every 5 minutes).

## 10. Connection lifecycle hooks

The SDK exposes hooks for observability:

```rust
client.on_connect(|server| log::info!("Connected to {}", server));
client.on_disconnect(|server| log::warn!("Disconnected from {}", server));
client.on_handshake_failure(|server, err| log::error!("Handshake failed: {}", err));
```

These integrate with the application's logging / metrics.

## 11. TLS configuration

TLS is optional but recommended for production:

```rust
let client = Client::builder()
    .tls(TlsConfig {
        ca_cert: Some(load_ca_pem("/etc/ssl/ca.pem")?),
        client_cert: Some(load_pem("/etc/ssl/client.pem")?),
        client_key: Some(load_pem("/etc/ssl/client-key.pem")?),
    })
    .build();
```

The TLS implementation uses the language's standard library (`rustls` for Rust, OpenSSL bindings elsewhere).

## 12. Authentication

Multiple methods:

- Token: a bearer token.
- Mutual TLS: client cert authenticates.
- API key: a per-application key.
- (Custom): pluggable auth.

```rust
let client = Client::builder()
    .auth(AuthMethod::Token("eyJ..."))
    .build();
```

The auth credential is sent on connection establishment, not per-request.

## 13. Connection limits

To prevent connection exhaustion:

- Max total connections (default 32).
- Max per server (default 8).
- Max queued requests waiting for a connection (default 1024).

If queued requests exceed the limit, new requests fail fast with `Overloaded` (a client-side overload, not a substrate one).

## 14. The "graceful close"

When the Client is dropped:

```
1. New requests fail with ClientClosed.
2. In-flight requests are awaited (with timeout).
3. Connections are gracefully closed (FIN).
4. Resources released.
```

The user can also explicitly close:

```rust
client.close().await?;
```

## 15. The "shutting down server" handling

The substrate may indicate it's shutting down:

- Sends `SHUTDOWN` frame.
- The SDK marks the connection as draining.
- New requests for this server go elsewhere.
- In-flight requests are given a chance to complete.

Graceful shutdown reduces error rates during maintenance.

## 16. Connection metrics

The SDK exposes:

- Connection count (per server, total).
- Connection age.
- Bytes sent/received.
- Frames sent/received.
- Errors (per type).

Surface via the SDK's metrics interface.

## 17. The "never disconnect" anti-pattern

Some SDKs aggressively reconnect on any error. This can amplify problems (the SDK floods the network with reconnects).

Brain's SDK uses exponential backoff for reconnects:

- First reconnect attempt: immediate.
- Second: 100ms delay.
- Third: 500ms.
- ... up to 30s max.

Backoff resets on successful reconnect.

## 18. The "circuit breaker" option

For applications wanting circuit-breaker behavior:

```rust
let client = Client::builder()
    .circuit_breaker(CircuitBreaker {
        failure_threshold: 10,
        reset_timeout: Duration::from_secs(60),
    })
    .build();
```

After failure_threshold consecutive failures, the SDK opens the circuit; new requests fail fast for reset_timeout. After timeout, circuit half-opens; one request tested.

This integrates with the application's circuit-breaker patterns.

---

*Continue to [`04_retries.md`](04_retries.md) for retries.*
