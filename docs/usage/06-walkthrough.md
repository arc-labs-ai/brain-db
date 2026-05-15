# 06 — End-to-end walkthrough

`crates/brain-sdk-rust/examples/store_and_recall.rs` is the
worked example: 30 memories across 8 domains, typed edges,
transactions, deduplication, RECALL across cues, FORGET, post-write
recall, SDK metrics — all against a live server.

This page walks through running it and reading the output.

## Setup

You'll need two terminals inside the same container.

**Terminal 1 — start the server:**

```bash
just docker-shell
# inside container:
cargo run --bin brain-server -- --config config/dev.toml
```

Wait for the `"listening"` log line.

**Terminal 2 — run the example:**

```bash
just docker-shell
# inside container:
cargo run --example store_and_recall -p brain-sdk-rust
```

## What the example does

8 phases, all driven by the Rust SDK against the running server:

| Phase | What |
|---|---|
| 1 | **ENCODE** — 30 memories across 8 domains (ML/AI, Physics, Biology, Philosophy, History, Software Engineering, Math, Linguistics) |
| 2 | **LINK** — 8 typed edges building a small knowledge graph |
| 3 | **TRANSACTION** — atomic cross-domain pair (quantum computing × Shor's algorithm) |
| 4 | **DEDUP** — re-encode an existing memory; server returns the same `memory_id` |
| 5 | **RECALL** — 8 queries with different cues and domains |
| 6 | **FORGET** — soft tombstone + hard zero-wipe; verifies the difference |
| 7 | **DURABILITY** — RECALL after writes to prove WAL persistence |
| 8 | **METRICS** — point-in-time SDK request / error counters |

## Expected output (abridged)

```
Connecting to Brain at 127.0.0.1:9090 ...
Connected.

──────────────────────────────────────────────────────────────────────
  PHASE 1 · ENCODE — 30 memories across 8 domains
──────────────────────────────────────────────────────────────────────

  [ML / AI  context=1]
    + attention mechanism  id=0x00010000000000010000000100000001
    + BERT                 id=0x00010000000000020000000100000001
    + transformer parallel id=0x00010000000000030000000100000001
    + GPT decoder-only     id=0x00010000000000040000000100000001
    + gradient momentum    id=0x00010000000000050000000100000001

  [Physics  context=2]
    + special relativity   id=0x00010000000000060000000100000001
    ...

  [Software Engineering  context=6]
    + CAP theorem           id=0x...
    + ACID properties       id=0x...
    + event sourcing        id=0x...
    + 2PC                   id=0x...

  30 memories encoded across 8 domains.

──────────────────────────────────────────────────────────────────────
  PHASE 2 · LINK — build the knowledge graph
──────────────────────────────────────────────────────────────────────
  BERT  --[DerivedFrom]--> attention mechanism  (w=0.95)
  GPT   --[DerivedFrom]--> transformer parallel  (w=0.90)
  CRISPR --[Supports]--> mRNA vaccines  (w=0.70)
  CAP   --[References]--> ACID  (w=0.80)
  2PC   --[DerivedFrom]--> ACID  (w=0.85)
  EventSourcing --[Contradicts]--> ACID (mutable-state)  (w=0.60)
  Kant  --[SimilarTo]--> Descartes  (w=0.65)
  Penicillin --[Caused(chain)]--> modern vaccine research  (w=0.55)

  8 edges added.

──────────────────────────────────────────────────────────────────────
  PHASE 3 · TRANSACTION — atomic cross-domain pair
──────────────────────────────────────────────────────────────────────
  Transaction id=[31 4a ...]
  Committed:
    quantum computing (physics)  id=0x...
    Shor's algorithm (software)  id=0x...
  Shor's algo --[DerivedFrom]--> quantum computing  (w=0.92)

──────────────────────────────────────────────────────────────────────
  PHASE 5 · RECALL — 8 queries, different cues and domains
──────────────────────────────────────────────────────────────────────
  Q1 'transformer neural network architecture' — 5 result(s):
    [0] 0.9243  [Semantic]  A transformer encoder processes all input tokens in parallel...
    [1] 0.9101  [Semantic]  The attention mechanism allows neural networks to focus on...
    [2] 0.8876  [Semantic]  GPT models use a decoder-only transformer trained with...

  Q6 'distributed database consistency guarantees atomicity' — 4 result(s):
    [0] 0.9312  [Semantic]  ACID properties — atomicity, consistency, isolation, durability...
    [1] 0.9101  [Semantic]  The CAP theorem (Brewer, 2000) proves that a distributed...
    [2] 0.8654  [Semantic]  Two-phase commit (2PC) coordinates distributed transactions...
    [3] 0.8201  [Semantic]  Event sourcing persists state as an immutable, append-only log...

──────────────────────────────────────────────────────────────────────
  PHASE 8 · SDK METRICS — point-in-time snapshot
──────────────────────────────────────────────────────────────────────
  requests_total      = 51
  errors_total        = 0
  retries_total       = 0
  encode.requests     = 32
  encode.errors       = 0
  recall.requests     = 9
  forget.requests     = 2

──────────────────────────────────────────────────────────────────────
  Done. Session closed.
  To inspect the server state, run in another terminal:
    just cli --output json debug-snapshot --shard 0 | jq .
    just cli --output json worker list
    curl http://127.0.0.1:9091/metrics | grep brain_
──────────────────────────────────────────────────────────────────────
```

## Validation checklist

After the example finishes:

- [ ] Process exited with code 0.
- [ ] `Phase 1` printed 30 ENCODE lines.
- [ ] `Phase 2` printed 8 LINK lines.
- [ ] `Phase 3` printed a transaction ID and two committed memories.
- [ ] `Phase 5` queries returned ≥ 3 results each with the top
      `similarity_score > 0.85`.
- [ ] `Phase 8` reported `errors_total = 0`.

Now in a third terminal, validate via the admin CLI / metrics:

```bash
just cli --output json worker list | jq '.workers | length'
```

Should print the worker count (12 × shard_count = 48 for the
default 4-shard config).

```bash
curl -s http://127.0.0.1:9091/metrics | grep "brain_request_total{op=\"encode\""
```

You should see counters reflecting the ENCODEs from the example.

```bash
just cli --output json debug-snapshot --shard 0 \
  | jq '.workers[] | select(.cycles > 0) | {name, cycles}'
```

Should list workers that have run at least once.

## Score interpretation

- **> 0.90** — strong semantic match; almost certainly the
  intended memory.
- **0.85–0.90** — relevant; surface for the user with confidence.
- **0.70–0.85** — related; useful for context but not definitive.
- **< 0.60** — unrelated, or the HNSW index is still warming up
  after the first encode batch (re-run RECALL after ~5s for
  stable scores).

## Re-running

The example is idempotent on dedup memories but the LINK/TRANSACTION
phases create new edges on each run. To start completely fresh:

```bash
# Terminal 1 (server): Ctrl+C
rm -rf ./data
cargo run --bin brain-server -- --config config/dev.toml
# Terminal 2:
cargo run --example store_and_recall -p brain-sdk-rust
```

## Writing your own quick script

Drop a file into `crates/brain-sdk-rust/examples/`:

```rust
// crates/brain-sdk-rust/examples/try.rs
use brain_sdk_rust::Client;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = Client::connect("127.0.0.1:9090".parse()?).await?;

    let e = client.encode("Paris is the capital of France.").send().await?;
    println!("stored: {:#x}", e.memory_id);

    let results = client.recall("French capital city").send().await?;
    for r in &results {
        println!("  {:.3}  {}", r.similarity_score, r.text);
    }

    client.bye().await?;
    Ok(())
}
```

Then:

```bash
cargo run --example try -p brain-sdk-rust
```

## Next

[`07-configuration.md`](07-configuration.md) — full config
reference.
