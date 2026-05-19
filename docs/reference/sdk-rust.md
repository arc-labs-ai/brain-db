# `brain-sdk-rust` reference

Public surface of the official Rust SDK. The crate at
`crates/brain-sdk-rust/` exposes a single `Client` over the
binary wire protocol on `listen_addr`.

For task-oriented walk-throughs see
[`../guides/sdk/`](../guides/sdk/). For the wire protocol itself
see [`wire-protocol/`](wire-protocol/).

**Source:** `crates/brain-sdk-rust/src/`. **Spec:** §13 (substrate
SDK), §29 (knowledge SDK).

## Runtime + transport

- **Runtime:** Tokio (multi-thread, `net` + `io-util` + `time` + `sync` features). No other runtime is supported; the SDK is `Send + Sync` and runs in any Tokio context.
- **Transport:** TCP to `listen_addr`. TLS handshake when the server has `[server.tls] enabled = true`.
- **Concurrency:** a `Client` holds a `Pool` of connections. One in-flight op per connection; multi-op concurrency comes from pool depth.

## Constructing a client

```rust
use brain_sdk_rust::{Client, ClientConfig, AuthMethod, PoolConfig, RetryConfig};
use std::net::SocketAddr;

// Simplest path — one connection, eager handshake, default config.
let client = Client::connect("127.0.0.1:8080".parse()?).await?;

// Explicit agent_id + config.
let client = Client::connect_with(
    "brain.example:8080".parse()?,
    agent_id,
    ClientConfig::default()
        .with_auth(AuthMethod::None)
        .with_timeout(Duration::from_secs(30))
        .with_pool(PoolConfig::default().with_min(2).with_max(16))
        .with_retry(RetryConfig::default()),
).await?;

// Lazy — no handshake until the first op.
let client = Client::new_lazy(addr, agent_id, ClientConfig::default());

// Warm the pool up explicitly.
client.warm_up().await?;
```

### `ClientConfig`

Builder, all fields optional:

| Method | Default | Notes |
|---|---|---|
| `with_auth(AuthMethod)` | `AuthMethod::None` | Only `None` is fully wired in v1. |
| `with_timeout(Duration)` | 30 s | Per-request wall-clock budget. |
| `with_pool(PoolConfig)` | min 1, max 8, idle 5 min | See below. |
| `with_retry(RetryConfig)` | 3 attempts, exponential 100 ms | See "Retry policy". |

### `PoolConfig`

| Method | Default | Notes |
|---|---|---|
| `.single()` | min 1, max 1 | Preset for the legacy `Client::connect` shape. |
| `.with_min(u32)` | 1 | Pool floor. Pre-established at `warm_up()` time. |
| `.with_max(u32)` | 8 | Pool ceiling. `Overloaded` if all are checked out. |
| `.with_idle_timeout(Duration)` | 5 min | Idle reaper threshold. |
| `.with_acquire_timeout(Duration)` | 30 s | How long to wait for a free slot. |

## Verbs

All five cognitive verbs follow the same builder pattern:

```rust
client.<verb>(<args>)
    .field1(value)
    .field2(value)
    .send()           // → Result<Resp, ClientError>
    // or
    .send_stream()    // → Result<FrameStream<Item>, ClientError>  (recall / plan / reason / subscribe)
```

### `encode`

```rust
let resp = client.encode("Alice merged the auth-rewrite branch.")
    .context(7)
    .kind(MemoryKindWire::Episodic)
    .salience(0.6)
    .edges(vec![EdgeRequest { target: prev, kind: EdgeKindWire::FollowedBy, weight: 1.0 }])
    .deduplicate(false)
    .send()
    .await?;
println!("new memory: {:?}", resp.memory_id);
```

Returns `EncodeResponse { memory_id, was_deduplicated, salience, auto_edges_added, … }`.

`.deduplicate(true)` opts in to the per-shard fingerprint index
(spec §07/07 §6): on a content match within the same
`(agent_id, context_id)`, the existing `MemoryId` is returned and
`was_deduplicated = true`. Default `false` — sibling encodes of
identical text produce distinct memories. See
[`brain-shell.md`](brain-shell.md) for the operator-level
explanation of the three states (`off` / `miss` / `hit`).

### `recall`

```rust
// Collect form (everything in one Vec):
let hits: Vec<MemoryResult> = client.recall("auth rewrite")
    .top_k(20)
    .confidence_threshold(0.6)
    .include_text(true)
    .send()
    .await?;

// Stream form (lazy, demand-driven):
let mut stream = client.recall("auth rewrite")
    .top_k(1000)
    .send_stream()
    .await?;
while let Some(batch) = stream.next().await {
    for hit in batch? { /* ... */ }
}
```

### `plan`

```rust
let steps: Vec<PlanStep> = client.plan(PlanState::Text("we're here".into()),
                                       PlanState::Text("we want there".into()))
    .budget(PlanBudget { max_steps: 8, max_wall_time_ms: 5000, max_branches_explored: 256 })
    .send()
    .await?;
```

### `reason`

```rust
let inferences: Vec<InferenceStep> = client.reason(ObservationInput::Text(claim))
    .depth(3)
    .confidence_threshold(0.5)
    .max_inferences(20)
    .send()
    .await?;
```

### `forget`

```rust
let resp = client.forget(memory_id)
    .mode(ForgetMode::Soft)
    .send()
    .await?;
```

v1 ships single-id form. Batch + filter variants follow in
later releases (spec §13/02 §7).

### `link` / `unlink`

```rust
client.link(source, EdgeKindWire::Causes, target).weight(0.8).send().await?;
client.unlink(source, EdgeKindWire::Causes, target).send().await?;
```

### `subscribe`

```rust
// Collect-form: capture up to N events, then stop.
let events = client.subscribe()
    .contexts(vec![7])
    .kinds(vec![MemoryKindWire::Episodic])
    .collect(100)
    .await?;

// Stream-form: long-lived async iterator.
let mut stream = client.subscribe()
    .start_lsn(snapshot_lsn + 1)
    .max_inflight(128)
    .send_stream()
    .await?;
while let Some(batch) = stream.next().await {
    for event in batch? { /* ... */ }
}
```

### Transactions

```rust
let txn = client.txn_begin().await?;
client.encode("...").txn(txn.txn_id).send().await?;
client.link(a, EdgeKindWire::SupportedBy, b).txn(txn.txn_id).send().await?;
client.txn_commit(txn.txn_id).await?;
```

Variants:
- `txn_begin_with_timeout(seconds)` to override the default 60 s wall-time.
- `txn_abort(txn_id)` to roll back.

## Knowledge layer

Active when the server has a schema declared.

### Entities

```rust
use brain_sdk_rust::{Person, PersonAttributes};

let entity_client = client.entity::<Person>();

let handle = entity_client.create()
    .canonical_name("Alice Singh")
    .alias("a.singh")
    .with_email("alice@example.com")
    .with_role("staff engineer")
    .send()
    .await?;

let by_id   = entity_client.get(handle.id).await?;
let updated = entity_client.update(handle.id).with_role("principal").send().await?;

// Resolve canonical name to an existing entity.
let resolve = entity_client.resolve("alice singh").send().await?;
println!("primary = {:?}", resolve.primary);

// Merge two duplicate entities.
let merged = entity_client.merge(primary, duplicate).send().await?;

// Tombstone (soft delete with reason).
entity_client.tombstone(handle.id, TombstoneReason::Duplicate).await?;
```

### Custom entity types

```rust
use brain_sdk_rust::BrainEntityType;

#[derive(BrainEntity)]
#[brain(entity_type = "Project")]
struct Project {
    code: String,
    title: String,
    started_at: Option<chrono::NaiveDate>,
}
```

The `#[derive(BrainEntity)]` macro maps your struct onto the
schema's `Project` entity. Field names + types must match the
schema declaration.

### Statements + relations + queries

Higher-level wrappers (`StatementsClient`, `RelationsClient`,
`QueryBuilder`) ship over phases 17–24. Wire-level access via
`StatementCreateReq (0x0140)` / `RelationCreateReq (0x0150)` /
`QueryReq (0x0160)` is available now via the connection
directly.

## Retry policy

```rust
let retry = RetryConfig::default()    // 3 attempts, 100 ms initial, exponential
    .with_max_attempts(5)
    .with_initial_delay(Duration::from_millis(50));
```

The SDK retries automatically on these `ClientError` variants:

| Variant | Reason |
|---|---|
| `Connect(_)` | TCP failed (refused / DNS / unreachable). |
| `Io(_)` | Socket-level I/O after handshake. |
| `Overloaded { .. }` | Pool at max; no free slot before `acquire_timeout`. |

Each retry **reuses the same `request_id`** — the server's 24 h
idempotency cache means retries are safe even for writes.

Non-retryable variants surface immediately: `Server { code, .. }`,
`Closed`, `Protocol(_)`, `PoolClosed`, `Auth(_)`, `Handshake(_)`.

### Liveness: four layers of defense

Brain's SDK keeps long-lived connections healthy with four cooperating
mechanisms, in order from cheapest to most active:

| Layer | What it catches | Mechanism | Detection budget |
|---|---|---|---|
| **1. Kernel TCP keepalive** (`SO_KEEPALIVE` + `TCP_KEEPIDLE/INTVL/CNT`) | Half-broken peers: NAT timeout, route loss, server power-cut without FIN | Set on every socket in `Connection::open`. Linux: idle 30 s, interval 10 s, retries 3 → ~60 s. macOS / Windows: same idle + interval, OS-default retries (~80 s). | ~60 s |
| **2. App-level CLIENT_PONG** (spec §03/02 §6.1) | Server's idle-close cycle. Server emits `SERVER_PING` after `idle_timeout` (300 s default); without a `CLIENT_PONG` within `ping_timeout` (30 s), it closes the connection. | `IdleConnection` background tokio task auto-responds to every `SERVER_PING` with a `CLIENT_PONG` that echoes the server timestamp. Pool connections survive arbitrary idle. | Instant (responds within ms of receiving the ping) |
| **3. Pool slot discard** | Connection that died for any reason despite layers 1–2 (rare) | `PoolGuard::mark_failed()` on `Io`/`Closed`/`Protocol`. `Drop` transitions slot → `Closed` instead of recycling. | Op-time |
| **4. Retry policy** | Recovery surface for layer 3 | `RetryConfig` default: 3 attempts, exponential backoff. Same `request_id` reused — server's 24 h idempotency cache makes writes safe to retry. | Up to ~600 ms |

Combined behaviour: any normal-operations scenario (NAT timeout, server
restart, brief network glitch, idle past the server's ping window)
recovers transparently. The first op after a recovery may add ~50 ms
of re-handshake latency; subsequent ops run at normal speed.

This catches up to the design space that **gRPC** ([gRPC keepalive
guide](https://grpc.io/docs/guides/keepalive/)) and **NATS** ([NATS
PING/PONG](https://docs.nats.io/using-nats/developer/connecting/pingpong))
settled on: app-level PING/PONG layered over kernel keepalive. Brain
spec §03/02 §6.1 has always called for it; the SDK now honors it.

### Deferred (not in v1)

- **Bidirectional `CLIENT_PING`** (NATS-style — client also probes the
  server, not just responds): currently the server detects a dead
  client via timeout; the SDK detects a dead server via Io error on
  next op (+ kernel keepalive). Adding client-initiated PINGs would
  detect a *slow* but reachable server faster, useful when Brain
  grows multi-shard server topologies.
- **Separate monitoring connections** (Mongo-style — a dedicated
  socket per server for liveness independent of op traffic): overkill
  for v1's single-server-per-Client pool. Reconsider with multi-shard
  federation.

### Regression tests for this design

| Test | What it proves |
|---|---|
| `tests/pool.rs::sdk_connection_has_so_keepalive_enabled` | Layer 1 — `SO_KEEPALIVE` is set on every Connection. |
| `tests/pool.rs::sdk_auto_responds_to_server_ping` | Layer 2 — a SERVER_PING received on an Idle pool slot produces a CLIENT_PONG with the echoed timestamp, within 2 s. |
| `tests/pool.rs::idle_connection_survives_a_burst_of_server_pings` | Layer 2 — bg task pongs every ping, not just the first. |
| `tests/pool.rs::pool_guard_mark_failed_discards_slot_on_drop` | Layer 3 — failed guard shrinks live slots; next acquire re-handshakes. |
| `tests/pool.rs::pool_guard_without_mark_failed_still_recycles` | Layer 3 — clean drops still recycle (no throughput regression). |

## Errors

```rust
#[non_exhaustive]
pub enum ClientError {
    Connect(io::Error),
    Handshake(String),
    Auth(String),
    Protocol(ProtocolError),
    Io(io::Error),
    Closed,
    Overloaded { detail: String },
    PoolClosed,
    Internal(String),
    Server { code: u16, message: String },
    RetryExhausted { last_error: Box<ClientError>, attempts: u32, total_duration: Duration },
}

impl ClientError {
    pub fn code(&self) -> Option<u16>;     // wire error code, if applicable
    pub fn is_retryable(&self) -> bool;
}
```

Wire error codes are the same taxonomy as
[`wire-protocol/error-codes.md`](wire-protocol/error-codes.md).

For knowledge-layer error dispatch:

```rust
use brain_sdk_rust::{ClientErrorEntityExt, EntityErrorKind};

if let Some(EntityErrorKind::NotFound) = err.entity_error_kind() {
    // ...
}
```

## Observability

```rust
let snap = client.metrics_snapshot();
println!("requests = {}", snap.requests_total);
println!("retries  = {}", snap.retries_total);
println!("in-flight = {}", snap.in_flight_gauge);
for (op, m) in &snap.per_op {
    println!("  {op}: req={} err={} retry={}", m.requests, m.errors, m.retries);
}
```

All counters are monotonic; callers compute deltas. The SDK also
emits `tracing` spans (`brain.encode`, `brain.recall`, etc.) for
distributed-tracing integration.

## Feature flags

The crate currently declares **no Cargo features** — every public
type is always exported. Future features will gate optional
extras (e.g. derive macros for custom entity types) without
moving anything that's already public.

## See also

- [`../guides/sdk/rust-quickstart.md`](../guides/sdk/rust-quickstart.md) — first-call walkthrough.
- [`wire-protocol/`](wire-protocol/) — what the SDK speaks.
- [`cognitive-operations/`](cognitive-operations/) — semantics of each verb.

**Spec:** §13 (substrate SDK), §29 (knowledge SDK). **Source:**
`crates/brain-sdk-rust/src/`.
