# Install Brain

Brain is **Linux-only** (kernel ≥ 5.15). Spec §01/05 §1.1 documents
why: `io_uring`, `O_DIRECT`, `pwritev2(RWF_DSYNC)`, and the
Linux-specific `madvise` / `fallocate` flags Brain depends on don't
have portable equivalents.

## Two install paths

| Path | Use when |
|---|---|
| **Native binary** | Production deployments on Linux x86_64 / aarch64 hosts |
| **Docker / OrbStack** | Development on macOS / Windows hosts |

## Native binary

### From source

Brain is a Rust workspace. Build the server + CLI with stable
Rust (MSRV: latest minus one, currently 1.95):

```bash
git clone https://github.com/brain-db-io/brain-db
cd brain-db
cargo build --workspace --release
```

The three binaries you'll want on the host:

- `target/release/brain-server` — the substrate daemon.
- `target/release/brain` — the interactive shell (`psql` / `redis-cli` equivalent). REPL + one-shot cognitive ops over the wire protocol. See [`../reference/brain-shell.md`](../reference/brain-shell.md).
- `target/release/brain-cli` — the admin CLI. HTTP `/v1/*` routes (snapshots, audit, worker control, health, metrics). See [`../reference/cli.md`](../reference/cli.md).

Copy them somewhere on `$PATH`:

```bash
sudo install -m 755 target/release/brain-server /usr/local/bin/
sudo install -m 755 target/release/brain        /usr/local/bin/
sudo install -m 755 target/release/brain-cli    /usr/local/bin/
```

### From pre-built release

(Not yet shipped — v1.0.0 release will publish `.tar.gz` artifacts
for x86_64 / aarch64 from a GitHub Release.)

## Docker / dev container

For development on macOS or Windows, use the in-repo dev container:

```bash
# OrbStack / Docker Desktop must be running.
devcontainer up --workspace-folder .
devcontainer exec --workspace-folder . cargo fmt --all -- --check
devcontainer exec --workspace-folder . cargo build --workspace --all-targets
devcontainer exec --workspace-folder . cargo clippy --workspace --all-targets -- -D warnings
devcontainer exec --workspace-folder . cargo test --workspace --all-targets
devcontainer exec --workspace-folder . ./scripts/check-skills.sh
```

The container shells out to the same image CI uses. Everything that
compiles in the container compiles on a real Linux host. See
[`docs/development/usage/`](../development/usage/) for the day-to-day
inner loop.

## Smoke test

After installing, verify the binary runs:

```bash
brain-server --version    # should print: brain-server <semver>
```

Start it against a temp data dir:

```bash
mkdir -p /tmp/brain-test/data
cat > /tmp/brain-test/config.toml <<'EOF'
[server]
listen_addr = "127.0.0.1:8080"
metrics_addr = "127.0.0.1:9091"    # /healthz + /metrics
admin_addr   = "127.0.0.1:9092"    # /v1/* admin routes (keep loopback)

[storage]
data_dir = "/tmp/brain-test/data"
shard_count = 1

[shard]
arena_capacity_bytes = "1GiB"
wal_segment_size_bytes = "64MiB"
wal_retention_segments = 4

[hnsw]
m = 16
ef_construction = 200
ef_search = 64

[embedder]
model = "bge-small-en-v1.5"
cache_size = 1000
batch_size = 32
batch_window_ms = 5

[auth]
mode = "none"
EOF

brain-server --config /tmp/brain-test/config.toml &
sleep 2
curl -s http://127.0.0.1:9091/healthz   # → "ok\n"
curl -s http://127.0.0.1:9091/metrics | head -10
kill %1
```

## Next steps

- **Configure for your workload**: [`configure.md`](configure.md)
- **Run in production**: [`operate.md`](operate.md)
- **Upgrade between versions**: [`upgrade.md`](upgrade.md)
- **Monitor**: [`observability.md`](observability.md)
