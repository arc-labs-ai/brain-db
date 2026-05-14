# Brain — Usage Guide

This guide covers everything you need to clone the repo, start the
development environment, run the server, and interact with it using
both the Rust SDK and the admin CLI. It is structured as a series of
concrete steps with exact commands and expected output.

---

## Prerequisites

| Tool | Install | Purpose |
|---|---|---|
| Docker Desktop 4.x+ | https://www.docker.com/products/docker-desktop | Container runtime |
| devcontainer CLI | `npm install -g @devcontainers/cli` | Manages the dev container |
| `just` (optional on host) | `cargo install just` | Task runner shortcut |
| Rust stable (optional on host) | https://rustup.rs | Only needed for host-side editing; all builds run in container |

Brain uses Linux-only kernel features — io_uring (Glommio executor),
O_DIRECT WAL writes, `pwritev2(RWF_DSYNC)` group commit. All runtime
work happens inside a Linux dev container. The container image is
`FROM rust:1-bookworm` with the memlock rlimit raised and seccomp set
to unconfined so io_uring syscalls are allowed.

---

## 1. Clone and container setup

```
git clone https://github.com/brain-db-io/brain-db.git
cd brain-db
```

Bring the container up. This builds the image on first run (2–5
minutes) and is idempotent on subsequent runs — it re-attaches to the
existing container without re-firing the build.

```
just docker-up
```

Expected output (first run):

```
[+] Building 47.3s (11/11) FINISHED
==> brain-dev container post-create
rustc 1.XX.0 (stable)
cargo 1.XX.0
just 1.XX.X
gh 2.XX.X
git 2.XX.X

==> Quick verify (skips cargo work; just lints conventions)
Container ready. Useful commands:
  just verify
  cargo test -p brain-protocol
  ...
```

Expected output (subsequent runs):

```
[+] Running 1/0
 Container brain-dev Running
```

The container mounts three persistent named volumes so incremental
build state survives restarts:

```
brain-cargo-registry   /usr/local/cargo/registry
brain-cargo-git        /usr/local/cargo/git
brain-target-cache     /workspaces/brain/target
```

---

## 2. Enter the container shell

```
just docker-shell
```

You will land at:

```
[brain-dev] /workspaces/brain$
```

All commands in sections 3 onwards run inside this shell unless noted.

Alternatively, run a single command without entering interactively:

```
just docker <command>

# example
just docker cargo check --workspace
```

---

## 3. Build the workspace

Inside the container:

```
cargo build --workspace
```

Or from the host:

```
just docker cargo build --workspace
```

First build downloads all dependencies (cached in named volumes after
this). Expected time: 3–8 minutes on first run, under 30 seconds on
subsequent runs.

Release build:

```
cargo build --workspace --release
```

Binaries land at:

```
target/debug/brain-server
target/debug/brain-cli
target/release/brain-server   (release build)
target/release/brain-cli      (release build)
```

---

## 4. Run the full verification suite

This is the gate every commit passes before merging. It runs format
check, all tests, and clippy with `-D warnings`.

```
just docker-verify
```

Expected output:

```
cargo fmt --all -- --check   (no output = clean)
cargo test --workspace       running N tests ... ok
cargo clippy ...             (no output = clean)
Finished dev profile in X.XXs
```

To run a subset:

```
# one crate's tests with output
just docker-test -p brain-protocol -- --nocapture

# clippy only
just docker-clippy

# specific test
just docker cargo test -p brain-server --test e2e
```

---

## 5. Run the server

### 5a. Using the dev config (simplest)

The dev config is at `config/dev.toml`. It binds three ports on
localhost:

| Port | Purpose | Used by |
|---|---|---|
| 9090 | Data plane (wire protocol, TCP) | SDK clients, agents |
| 9091 | Admin + Prometheus metrics (HTTP) | brain-cli, scraping |
| 9092 | Admin HTTP (additional admin endpoint) | brain-cli health / config |

Start the server:

```
cargo run --bin brain-server -- --config config/dev.toml
```

Or via the Justfile shortcut:

```
just run-server
```

Expected startup log (JSON format per dev.toml):

```json
{"timestamp":"...","level":"INFO","message":"brain-server starting","version":"0.1.0","shards":4}
{"timestamp":"...","level":"INFO","message":"admin server bound","addr":"127.0.0.1:9091"}
{"timestamp":"...","level":"INFO","message":"admin server accepting","addr":"127.0.0.1:9091"}
{"timestamp":"...","level":"INFO","message":"listening","listen":"127.0.0.1:9090","metrics":"127.0.0.1:9091","admin":"127.0.0.1:9092","shards":4,"data_dir":"./data"}
```

The server is ready when the `listening` line appears.

### 5b. Overriding config via environment variables

Any TOML field can be overridden with `BRAIN__SECTION__FIELD=value`.
Double underscores separate nesting levels.

```bash
BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:9090 \
BRAIN__STORAGE__SHARD_COUNT=8 \
BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB \
cargo run --bin brain-server -- --config config/dev.toml
```

### 5c. Data directory

The server writes shard data to `data/` relative to the working
directory by default. The directory is created automatically. Each
shard gets its own subdirectory:

```
data/
  shard-0/
    arena.bin
    wal-000001.seg
    metadata.redb
  shard-1/
  ...
```

To start fresh, remove the data directory:

```
rm -rf ./data
```

---

## 6. Interact via brain-cli

`brain-cli` connects to the admin HTTP server (port 9091 by default).

Global flags available on every command:

```
--server <host:port>     Admin endpoint (default 127.0.0.1:9091)
--output <json|table>    Output format (default table)
--shard <N>              Target a specific shard (0-indexed)
```

### Health check

```
just cli health
```

Table output:

```
status            healthy
admin_endpoint    127.0.0.1:9091
probe             /healthz
```

JSON output:

```
just cli --output json health
```

```json
{
  "status": "healthy",
  "admin_endpoint": "127.0.0.1:9091",
  "probe": "/healthz"
}
```

### Prometheus metrics snapshot

```
just cli stats
```

```
brain_up                     1
brain_shards_total           4
brain_connections_active     0
brain_connections_total      12
process_uptime_seconds       847
...
```

### Shard list

```
just cli shard list
```

```
index 0    shard_id=0
index 1    shard_id=1
index 2    shard_id=2
index 3    shard_id=3
```

JSON:

```
just cli --output json shard list
```

```json
{"shards":[{"index":0,"shard_id":0},{"index":1,"shard_id":1},{"index":2,"shard_id":2},{"index":3,"shard_id":3}]}
```

### Worker list

Each shard runs 12 background workers. List them all:

```
just cli worker list
```

```
shard 0 / decay              cycles=14  processed=0   errors=0  last_run_unix=1715682000
shard 0 / access_boost       cycles=14  processed=0   errors=0  last_run_unix=1715682000
shard 0 / consolidation      cycles=2   processed=0   errors=0  last_run_unix=1715681400
...
```

Filter to one shard:

```
just cli --shard 0 worker list
```

### Read the loaded config

Full config as JSON:

```
just cli --output json config get
```

```json
{
  "server": {"listen_addr": "127.0.0.1:9090", "metrics_addr": "127.0.0.1:9091", ...},
  "storage": {"data_dir": "./data", "shard_count": 4},
  "hnsw": {"m": 16, "ef_construction": 200, "ef_search": 64},
  ...
}
```

Specific key (dotted path):

```
just cli --output json config get --key hnsw.m
```

```json
16
```

```
just cli --output json config get --key workers.decay_interval_sec
```

```json
3600
```

### Snapshot a shard

Create a snapshot of shard 0:

```
just cli --shard 0 snapshot create
```

```
id       1715682345
shard    0
```

List snapshots:

```
just cli snapshot list
```

```
shard 0 / snapshot 1715682345    1048576 bytes, taken_at_unix_nanos=1715682345000000000
```

Delete a snapshot by id:

```
just cli --shard 0 snapshot delete 1715682345
```

### Rebuild the HNSW index for a shard

Forces an immediate out-of-schedule rebuild of the ANN index:

```
just cli --shard 0 rebuild-ann
```

```
shard       0
entries     42891
elapsed_ms  3241
```

### Debug snapshot

Captures the current runtime state of a shard. In v1 the schema is
partial — worker statuses are populated, other fields are listed in
`deferred` and will be filled as later phases land.

```
just cli --output json debug-snapshot --shard 0
```

```json
{
  "shard": 0,
  "captured_at_unix": 1715682400,
  "partial": true,
  "deferred": ["active_tasks","pending_requests","recent_errors","in_memory_state_summary"],
  "workers": [
    {"name":"decay","cycles":14,"processed":0,"errors":0,"last_run_unix":1715682000},
    {"name":"hnsw_maintenance","cycles":2,"processed":0,"errors":0,"last_run_unix":1715681800}
  ]
}
```

Write to a file instead of stdout:

```
just cli --output json debug-snapshot --shard 0 --value /tmp/snap.json
cat /tmp/snap.json
```

### Using a remote server

All brain-cli commands accept `--server`:

```
just cli --server 10.0.0.5:9091 health
just cli --server 10.0.0.5:9091 --output json shard list
```

---

## 7. Interact via the Rust SDK

The SDK lives at `crates/brain-sdk-rust`. It connects to the data
plane port (9090 by default), handles the HELLO/AUTH handshake, and
exposes a builder API for every cognitive operation.

Add to your project's `Cargo.toml`:

```toml
[dependencies]
brain-sdk-rust = { git = "https://github.com/brain-db-io/brain-db.git" }
```

### Minimal example

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

### Pool configuration for production use

The default `Client::connect` uses a single connection. For
concurrent workloads configure the pool:

```rust
use brain_sdk_rust::{Client, ClientConfig};
use brain_sdk_rust::pool::PoolConfig;

let config = ClientConfig::new()
    .with_pool(
        PoolConfig::new()
            .with_min(4)
            .with_max(16)
    );

let client = Client::connect_with(addr, agent_id, config).await?;
```

### SDK operation reference

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

---

## 8. Storing and retrieving data (inside the container)

This section assumes you are already inside the dev container shell
(`just docker-shell`) and the server is running in a separate
terminal pane inside the same container.

### Start the server (terminal 1)

```
cargo run --bin brain-server -- --config config/dev.toml
```

Wait for the `listening` log line before proceeding.

### Run the built-in example (terminal 2)

A complete working example lives at
`crates/brain-sdk-rust/examples/store_and_recall.rs`. It encodes
five memories, recalls by cue text, demonstrates a transaction, and
forgets a throwaway memory.

```
cargo run --example store_and_recall -p brain-sdk-rust
```

Expected output:

```
Connecting to Brain at 127.0.0.1:9090 ...
Connected.

=== ENCODE ===
  STORED  id=0x00010000000000010000000100000001  deduped=false  text=The attention mechanism in transformers was introduced in...
  STORED  id=0x00010000000000020000000100000001  deduped=false  text=BERT uses bidirectional training of transformers for lan...
  STORED  id=0x00010000000000030000000100000001  deduped=false  text=GPT-3 demonstrated few-shot learning with 175 billion p...
  STORED  id=0x00010000000000040000000100000001  deduped=false  text=The softmax function converts raw scores into a probabil...
  STORED  id=0x00010000000000050000000100000001  deduped=false  text=Backpropagation computes gradients by the chain rule.

=== RECALL (cue: 'transformer architecture') ===
  [0] score=0.9121  kind=Semantic  id=0x00010000000000010000000100000001
       The attention mechanism in transformers was introduced in 'Attention is All You Need' (2017).
  [1] score=0.8843  kind=Semantic  id=0x00010000000000020000000100000001
       BERT uses bidirectional training of transformers for language understanding.
  [2] score=0.7234  kind=Episodic  id=0x00010000000000030000000100000001
       GPT-3 demonstrated few-shot learning with 175 billion parameters.

=== RECALL (semantic only, top 2) ===
  score=0.8102  The softmax function converts raw scores into a probability distribution.
  score=0.7899  Backpropagation computes gradients by the chain rule.

=== FORGET ===
  Encoded throwaway id=0x00010000000000060000000100000001
  Forgotten id=0x00010000000000060000000100000001  edges_removed=0

=== TRANSACTION ===
  Transaction started txn_id=[...]
  Pending A=0x00010000000000070000000100000001  B=0x00010000000000080000000100000001
  Committed.

=== SDK METRICS ===
  requests_total      = 10
  errors_total        = 0

Done. Connection closed.
```

Similarity scores and memory IDs will differ on each run. Scores
above 0.7 indicate strong relevance; below 0.5 indicates low signal.

### Write your own quick script

Create a file anywhere in the workspace, for example
`scratch/try.rs`:

```
mkdir -p scratch
cat > scratch/try.rs << 'EOF'
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
EOF
```

Then compile and run it directly with `rustc` against the SDK:

```
# Easiest: add it as a temporary example
cp scratch/try.rs crates/brain-sdk-rust/examples/try.rs
cargo run --example try -p brain-sdk-rust
rm crates/brain-sdk-rust/examples/try.rs
```

### What the memory_id encodes

Every `STORE` response returns a `memory_id` (u128). It encodes the
storage address and is not opaque — you can decode it:

```
shard   = memory_id >> 112          (top 16 bits)
slot    = (memory_id >> 64) & mask  (next 48 bits)
version = (memory_id >> 32) & mask  (next 32 bits)
```

The shard field tells you which of the four shard executors owns the
memory. The slot is the arena index. The version is a monotonically
increasing counter that makes stale references detectable.

### ENCODE in depth

```rust
client
    .encode("text to store")
    .kind(MemoryKindWire::Semantic)    // or Episodic
    .salience(0.9)                     // 0.0–1.0; higher = slower to decay
    .context(42)                       // group memories by context_id
    .deduplicate(true)                 // skip if fingerprint matches an existing memory
    .send()
    .await?
```

The server embeds the text with BGE-small-en-v1.5 (384 dimensions)
server-side. The caller never touches vectors.

### RECALL in depth

```rust
client
    .recall("cue text for similarity search")
    .send()
    .await?
// returns Vec<MemoryResult> sorted by similarity_score descending
```

Each `MemoryResult` contains:

```
memory_id            u128     storage address
text                 String   the original stored text
similarity_score     f32      cosine similarity to the cue (0.0–1.0)
confidence           f32      model confidence estimate
salience             f32      current salience after decay
kind                 enum     Episodic | Semantic
context_id           u64      the context group
created_at_unix_nanos u64     store timestamp
edges                Option   graph edges to other memories (if requested)
```

### FORGET in depth

Soft forget (default) creates a tombstone. The slot is reclaimed
after the tombstone grace period (default 7 days, spec §02/05).

```rust
use brain_protocol::request::ForgetMode;

client
    .forget(MemoryId::from_raw(memory_id))
    .mode(ForgetMode::Soft)   // tombstone; reclaimed after grace period
    // .mode(ForgetMode::Hard) // immediate zero-wipe; spec §09/06
    .send()
    .await?
```

### LINK — connecting memories

Memories are nodes in a directed graph. Add a typed edge:

```rust
use brain_protocol::request::{EdgeKindWire, MemoryKindWire};

// first store two memories
let a = client.encode("Paris is the capital of France.").send().await?;
let b = client.encode("The Eiffel Tower is in Paris.").send().await?;

// link b -> a with a "located_in" relationship
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

### TRANSACTION — atomic multi-write

```rust
let txn = client.txn_begin().await?;

let a = client
    .encode("atomic write A")
    .txn(txn.txn_id)
    .send()
    .await?;

let b = client
    .encode("atomic write B")
    .txn(txn.txn_id)
    .send()
    .await?;

// both land atomically or neither does
client.txn_commit(txn.txn_id).await?;
// or: client.txn_abort(txn.txn_id).await?;
```

### Verify data is persisted

After encoding some memories, stop the server and restart it:

```
# in terminal 1: ctrl+c
cargo run --bin brain-server -- --config config/dev.toml
```

Then recall again. The memories are durable — WAL records were
fsynced before the ENCODE returned, and the arena + metadata
are replayed on restart.

### Inspect via admin CLI while data flows

While the example runs in terminal 2, in terminal 3:

```
# watch shard worker progress
just cli worker list

# capture a runtime snapshot of shard 0
just cli --output json debug-snapshot --shard 0 | jq '{workers: [.workers[] | {name,cycles}]}'

# watch metrics increment as encodes land
just cli stats | grep brain_connections
```

---

## 9. Ports and network map (from inside the container)

```
                 ┌──────────────────────────────────┐
                 │            brain-server           │
                 │                                   │
 SDK / agents    │  9090  data plane (wire protocol) │
 ──────────────► │        TCP, binary rkyv frames    │
                 │                                   │
 brain-cli       │  9091  admin + metrics (HTTP)     │
 Prometheus      │        GET /healthz               │
 ──────────────► │        GET /metrics               │
                 │        GET /v1/workers            │
                 │        GET /v1/config             │
                 │        GET /v1/shards             │
                 │        GET /v1/diagnostics/...    │
                 │        POST /v1/snapshots         │
                 │        POST /v1/rebuild-ann       │
                 └──────────────────────────────────┘
```

To expose ports from the container to the host, pass `-p` flags to
`docker run` or add `forwardPorts` to `.devcontainer/devcontainer.json`:

```json
"forwardPorts": [9090, 9091]
```

Then rebuild the container:

```
just docker-rebuild
```

---

## 10. Logging and debugging

### Log level

The server uses `tracing` with JSON output by default. The log level
is set in `config/dev.toml` under `[logging]`. Override at runtime:

```bash
RUST_LOG=brain_server=debug cargo run --bin brain-server -- --config config/dev.toml
RUST_LOG=brain_storage=trace,brain_server=info cargo run ...
```

Valid levels: `error`, `warn`, `info`, `debug`, `trace`.

### Reading structured logs

Log lines are JSON objects. Pipe through `jq` to filter:

```bash
cargo run --bin brain-server -- --config config/dev.toml 2>&1 | jq 'select(.level == "ERROR")'
cargo run ... 2>&1 | jq 'select(.fields.shard != null) | {shard: .fields.shard, msg: .fields.message}'
```

### Runtime debug snapshot

The debug-snapshot command (section 6) gives a point-in-time view of
worker state without stopping the server:

```
just cli --output json debug-snapshot --shard 0 | jq '.workers[] | select(.errors > 0)'
```

### Prometheus scraping

The `/metrics` endpoint exposes Prometheus text-format output:

```
curl http://127.0.0.1:9091/metrics
```

Key metrics:

```
brain_up                       1 when accepting requests
brain_shards_total             number of configured shards
brain_connections_active       current in-flight client connections
brain_connections_total        total accepted since startup
brain_worker_cycles_total      worker run count per worker per shard
brain_worker_errors_total      worker error count per worker per shard
brain_worker_last_run_unixtime unix timestamp of last worker cycle
process_uptime_seconds         uptime since server start
```

### Backtrace on panic

The container sets `RUST_BACKTRACE=1` automatically. For a full
backtrace:

```bash
RUST_BACKTRACE=full cargo run --bin brain-server -- --config config/dev.toml
```

### Per-crate test output

Run a single test with full output:

```
cargo test -p brain-storage --lib -- arena::tests::crc_mismatch_halts --nocapture
```

### Miri (unsafe memory safety)

Run the `brain-storage` unsafe blocks under Miri. Syscall-bound paths
are excluded (mmap, pwritev2); the ~47 pure-data tests run:

```
just miri
```

---

## 11. Configuration reference

Full schema is in `spec/01_system_architecture/`. The dev config at
`config/dev.toml` documents every field. Key sections:

### [server]

```toml
listen_addr = "127.0.0.1:9090"   # data plane TCP
metrics_addr = "127.0.0.1:9091"  # admin + metrics HTTP
admin_addr = "127.0.0.1:9092"    # additional admin HTTP
```

### [storage]

```toml
data_dir = "./data"    # shard data root
shard_count = 4        # number of shards; each pins a CPU core
```

### [shard]

```toml
arena_capacity_bytes = "1GiB"      # per-shard mmap arena
wal_segment_size_bytes = "256MiB"  # rotate WAL at this size
wal_retention_segments = 4         # keep last N segments
```

### [hnsw]

```toml
m = 16                  # max edges per node (spec §06/02)
ef_construction = 200   # candidate list during index build
ef_search = 64          # candidate list during recall
```

### [embedder]

```toml
model = "bge-small-en-v1.5"  # HuggingFace model id
cache_size = 10000            # in-memory embedding cache entries
batch_size = 32               # max texts per embedding batch
batch_window_ms = 5           # wait up to Nms to form a batch
```

The model is downloaded from HuggingFace on first startup. Subsequent
starts use a local cache. Allow a few minutes on the first run.

### [auth]

```toml
mode = "none"    # dev: no auth
# mode = "api_key"  # production: require token header
```

### Environment variable overrides

Any field can be overridden with `BRAIN__SECTION__FIELD=value`:

```bash
BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:9090
BRAIN__STORAGE__SHARD_COUNT=8
BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB
BRAIN__HNSW__EF_SEARCH=128
BRAIN__LOGGING__LEVEL=debug
```

---

## 12. Container lifecycle cheatsheet

| What you want | Command |
|---|---|
| Start container (first time or after restart) | `just docker-up` |
| Enter interactive shell | `just docker-shell` |
| Run one command without entering | `just docker <cmd>` |
| Stop container (keeps volumes) | `just docker-stop` |
| Remove container (keeps volumes) | `just docker-down` |
| Full rebuild (Dockerfile or config changed) | `just docker-rebuild` |
| Remove volumes too (nuclear reset) | `docker volume rm brain-cargo-registry brain-cargo-git brain-target-cache` |

---

## 13. Common issues

### io_uring permission denied

The container needs `--ulimit memlock=-1` and `--security-opt seccomp=unconfined`.
These are set in `.devcontainer/devcontainer.json`. If you see an
io_uring setup error, the container was started without these flags.
Run `just docker-rebuild` to recreate it from the devcontainer spec.

### Port already in use

Another process is holding 9090/9091. Find and stop it:

```bash
lsof -i :9090
lsof -i :9091
kill <PID>
```

Or change the ports in `config/dev.toml` and restart the server.

### Model download fails on first startup

BGE-small is fetched from HuggingFace on the first ENCODE request. If
the download fails (network issue, proxy, rate-limit), the server logs
an error and the ENCODE returns an error frame. Retry after the
network issue resolves; the partial download is not cached.

To pre-download inside the container:

```bash
# inside container shell
python3 -c "
from huggingface_hub import snapshot_download
snapshot_download('BAAI/bge-small-en-v1.5')
"
```

### HNSW test flake

One HNSW unit test (`hnsw::tests::*`) occasionally fails under high
parallel load in the container. This is a known race in the
`hnsw_rs` crate under parallel test execution, not a regression in
Brain. Run `just docker-verify` again; it passes on retry.

### Clean build required

If the build produces inexplicable linker errors after a major
dependency change:

```bash
# inside container
cargo clean
cargo build --workspace
```

The target volume is preserved but its contents are cleared.

---

## 14. Running the e2e test suite

The `brain-server` crate has three integration suites that start a
real in-process server and drive it via the SDK and CLI:

```
# raw wire smoke (e2e.rs)
just docker cargo test -p brain-server --test e2e

# SDK round-trips (sdk_e2e.rs)
just docker cargo test -p brain-server --test sdk_e2e

# CLI lib-level (cli_e2e.rs)
just docker cargo test -p brain-server --test cli_e2e
```

These tests require Linux (io_uring) and will report zero tests on
macOS — that is expected.

---

## 15. Project layout quick-reference

```
spec/                    218-file design specification (read-only)
config/dev.toml          Server configuration for local dev
ROADMAP.md               17-phase build plan
AUTONOMY.md              AI operating contract
CLAUDE.md                Claude Code session context
docs/phases/             Per-phase sub-task docs and exit checklists
.claude/plans/           Pre-implementation plan files (one per sub-task)
.claude/skills/          Project-local Claude Code skill definitions
.devcontainer/           Docker dev environment spec
crates/
  brain-core/            Shared types: MemoryId, EdgeKind, Error
  brain-protocol/        Wire protocol: frames, opcodes, codec
  brain-storage/         Arena + WAL + recovery (unsafe mmap allowed here only)
  brain-metadata/        redb wrapper, schema
  brain-index/           HNSW integration
  brain-embed/           BGE embedding service
  brain-planner/         Query planner + executor
  brain-ops/             Cognitive operations
  brain-workers/         12 background workers
  brain-server/          Server binary
  brain-sdk-rust/        Rust client SDK
  brain-cli/             Admin CLI
```
