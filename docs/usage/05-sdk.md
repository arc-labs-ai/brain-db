# 05 — Rust SDK

`brain-sdk-rust` connects to the data plane port (9090 by default),
handles the HELLO/AUTH handshake, and exposes a builder API for
every cognitive operation.

## Add to your project

```toml
[dependencies]
brain-sdk-rust = { git = "https://github.com/brain-db-io/brain-db.git" }
brain-core     = { git = "https://github.com/brain-db-io/brain-db.git" }
brain-protocol = { git = "https://github.com/brain-db-io/brain-db.git" }
tokio          = { version = "1", features = ["full"] }
anyhow         = "1"
```

## Minimal example

```rust
use std::net::SocketAddr;
use brain_sdk_rust::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let addr: SocketAddr = "127.0.0.1:9090".parse()?;

    // Opens one connection, completes HELLO/AUTH handshake.
    let client = Client::connect(addr).await?;

    // ENCODE — text is embedded server-side (BGE-small-en-v1.5).
    let encode = client
        .encode("The attention mechanism was introduced in Vaswani et al. 2017.")
        .send()
        .await?;
    println!("encoded memory_id = {:#x}", encode.memory_id);

    // RECALL — similarity search, returns up to top_k results.
    let results = client
        .recall("transformer attention")
        .send()
        .await?;
    for r in &results {
        println!("  score={:.3}  text={}", r.similarity_score, r.text);
    }

    // FORGET — soft tombstone; the slot is reclaimed after grace period.
    use brain_core::MemoryId;
    use brain_protocol::request::ForgetMode;
    let memory_id = MemoryId::from_raw(encode.memory_id);
    client.forget(memory_id).mode(ForgetMode::Soft).send().await?;

    client.bye().await?;
    Ok(())
}
```

**Verify:**

- `encode.memory_id` is non-zero (top bits encode the shard, see
  [§ memory_id encoding](#what-the-memory_id-encodes) below).
- `results` is non-empty with the first entry's `similarity_score`
  > 0.85.
- The process exits cleanly with exit code 0.

## Pool configuration

The default `Client::connect` uses a single connection. For
concurrent workloads:

```rust
use brain_sdk_rust::{Client, ClientConfig};
use brain_sdk_rust::pool::PoolConfig;

let config = ClientConfig::new()
    .with_pool(PoolConfig::new().with_min(4).with_max(16));

let client = Client::connect_with(addr, agent_id, config).await?;
```

## Operation reference

| Method | Description | Returns |
|---|---|---|
| `client.encode(text)` | Store a memory | `EncodeResponse { memory_id, salience, ... }` |
| `client.recall(cue)` | Similarity search | `Vec<MemoryResult>` |
| `client.forget(memory_id)` | Tombstone a memory | `ForgetResponse` |
| `client.link(from, to, kind)` | Add a directed edge | `LinkResponse` |
| `client.unlink(from, to, kind)` | Remove an edge | `UnlinkResponse` |
| `client.plan(start, goal)` | Graph path plan | streaming `Vec<PlanStep>` |
| `client.reason(observation)` | Derive inferences | streaming `Vec<InferenceStep>` |
| `client.subscribe()` | Event stream | async `FrameStream<EventEnvelope>` |
| `client.txn_begin()` | Start a transaction | `TxnBeginResponse { txn_id }` |
| `client.txn_commit(txn_id)` | Commit | `TxnCommitResponse` |
| `client.txn_abort(txn_id)` | Rollback | `TxnAbortResponse` |

All methods return builders; call `.send().await` to execute.

## ENCODE in depth

```rust
use brain_protocol::request::MemoryKindWire;

client
    .encode("text to store")
    .kind(MemoryKindWire::Semantic)    // or Episodic
    .salience(0.9)                     // 0.0–1.0; higher = slower to decay
    .context(42)                       // group memories by context_id
    .deduplicate(true)                 // skip if fingerprint matches existing
    .send()
    .await?
```

The server embeds with BGE-small-en-v1.5 (384 dimensions)
server-side; the caller never touches vectors.

**Verify a duplicate isn't re-stored:**

```rust
let a = client.encode("Paris").deduplicate(true).send().await?;
let b = client.encode("Paris").deduplicate(true).send().await?;
assert_eq!(a.memory_id, b.memory_id, "dedup should return the same id");
```

## RECALL in depth

```rust
let results = client.recall("cue text for similarity search").send().await?;
// Vec<MemoryResult> sorted by similarity_score descending.
```

Each `MemoryResult` contains:

```
memory_id              u128     storage address
text                   String   the original stored text
similarity_score       f32      cosine similarity (0.0–1.0)
confidence             f32      model confidence estimate
salience               f32      current salience after decay
kind                   enum     Episodic | Semantic
context_id             u64      the context group
created_at_unix_nanos  u64      store timestamp
edges                  Option   graph edges (if requested)
```

**Verify scores are meaningful:**

After ENCODE `"Paris is the capital of France."`,
`recall("French capital")` should return that memory with
`similarity_score > 0.85`. Scores below 0.60 suggest either the
cue is unrelated or the HNSW index is still warming up — wait a
few seconds and try again.

## FORGET in depth

Soft forget (default) tombstones the memory. The slot is reclaimed
after the tombstone grace period (default 7 days; spec §02/05).
Hard forget zero-wipes the slot immediately.

```rust
use brain_protocol::request::ForgetMode;
use brain_core::MemoryId;

client
    .forget(MemoryId::from_raw(memory_id))
    .mode(ForgetMode::Soft)     // tombstone; reclaimed after grace
    // .mode(ForgetMode::Hard)   // immediate zero-wipe
    .send()
    .await?;
```

**Verify the memory is gone:**

After FORGET, `recall(<original cue>)` should no longer include
that memory. The metric `brain_hnsw_tombstone_count` should
increment.

## LINK / UNLINK — graph edges

```rust
use brain_protocol::request::EdgeKindWire;

let a = client.encode("Paris is the capital of France.").send().await?;
let b = client.encode("The Eiffel Tower is in Paris.").send().await?;

client
    .link(
        MemoryId::from_raw(b.memory_id),
        MemoryId::from_raw(a.memory_id),
        EdgeKindWire::LocatedIn,
    )
    .weight(1.0)
    .send()
    .await?;
```

Edge kinds (full set in `brain_protocol::request::EdgeKindWire`):
`DerivedFrom`, `Supports`, `Contradicts`, `Caused`, `References`,
`SimilarTo`, `LocatedIn`, …

**Verify the edge exists** — RECALL with the source memory's text;
the result's `edges` field (when populated) carries the link.

## Transactions — atomic multi-write

```rust
let txn = client.txn_begin().await?;

let a = client.encode("atomic write A").txn(txn.txn_id).send().await?;
let b = client.encode("atomic write B").txn(txn.txn_id).send().await?;

// Both land atomically or neither does.
client.txn_commit(txn.txn_id).await?;
// or: client.txn_abort(txn.txn_id).await?;
```

**Verify atomicity** — between `txn_begin` and `txn_commit`, the
new memories are not visible to RECALL. After `commit`, they
appear; after `abort`, neither appears.

## What the memory_id encodes

Every ENCODE response returns a `memory_id` (`u128`). It's not
opaque — the layout is:

```
shard   = memory_id >> 112          (top 16 bits)
slot    = (memory_id >> 64) & mask  (next 48 bits)
version = (memory_id >> 32) & mask  (next 32 bits)
```

- `shard` tells you which shard executor owns the memory.
- `slot` is the arena index.
- `version` is a monotonically-increasing counter that makes
  stale references detectable (`NotFound` if version doesn't
  match).

**Verify by decoding:**

```rust
let mid = encode.memory_id;
println!("shard={}, slot={}, version={}",
         mid >> 112,
         (mid >> 64) & ((1u128 << 48) - 1),
         (mid >> 32) & ((1u128 << 32) - 1));
```

## SDK metrics snapshot

The SDK keeps in-memory counters of requests / errors / retries:

```rust
let snap = client.metrics_snapshot();
println!("requests_total = {}", snap.requests_total);
println!("errors_total   = {}", snap.errors_total);
```

These are independent of the server's `/metrics` endpoint.

## Next

[`06-walkthrough.md`](06-walkthrough.md) — the full
`store_and_recall` example tour.
