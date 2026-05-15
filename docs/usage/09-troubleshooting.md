# 09 — Troubleshooting

Common problems and how to resolve them. For production-side
failure modes, see [`docs/runbooks/`](../runbooks/) instead.

## io_uring permission denied

**Symptom:**

```
Failed to create io_uring instance: Permission denied (os error 13)
```

The container needs `--ulimit memlock=-1` and
`--security-opt seccomp=unconfined`. These are set in
`.devcontainer/devcontainer.json`. If you see this error, the
container was started without those flags.

**Fix:**

```bash
just docker-rebuild
```

Recreates the container from the devcontainer spec.

**Verify:**

```bash
just docker bash -c "cat /proc/self/limits | grep memlock"
```

Should show `max locked memory  unlimited  unlimited`.

## Port already in use

**Symptom:**

```
Error: bind: Address already in use (os error 98)
```

Another process is holding 9090, 9091, or 9092.

**Fix:**

Find the conflicting process:

```bash
lsof -i :9090
lsof -i :9091
```

Either `kill <PID>`, or change the port in `config/dev.toml`:

```toml
[server]
listen_addr  = "127.0.0.1:19090"
metrics_addr = "127.0.0.1:19091"
admin_addr   = "127.0.0.1:19092"
```

Or override at runtime:

```bash
BRAIN__SERVER__LISTEN_ADDR=127.0.0.1:19090 \
BRAIN__SERVER__METRICS_ADDR=127.0.0.1:19091 \
  cargo run --bin brain-server -- --config config/dev.toml
```

**Verify:**

```bash
curl -s http://127.0.0.1:19091/healthz
```

→ `ok`

## Model download fails on first startup

**Symptom:**

```
Error embedding text: model download failed: <network error>
```

BGE-small is fetched from HuggingFace on the first ENCODE. If the
download fails (network, proxy, rate limit), ENCODE returns an
error frame and subsequent retries also fail until the network
issue resolves.

**Fix — pre-download inside the container:**

```bash
python3 -c "
from huggingface_hub import snapshot_download
snapshot_download('BAAI/bge-small-en-v1.5')
"
```

**Verify:**

```bash
ls ~/.cache/huggingface/hub/models--BAAI--bge-small-en-v1.5/
```

Should show `snapshots/<hash>/` with the model files. Retry an
ENCODE; it should succeed.

## HNSW test flake

**Symptom:**

One of `brain-index`'s HNSW tests (e.g.
`hnsw::tests::tombstoned_memories_excluded_from_search`) fails
under high parallel load:

```
test hnsw::tests::tombstoned_memories_excluded_from_search ... FAILED
```

This is a known race in the `hnsw_rs` crate under parallel test
execution — not a regression in Brain. Tests pass standalone.

**Fix:**

```bash
just docker-verify
```

Re-run. Passes on retry. To run brain-index in isolation:

```bash
just docker-test -p brain-index
```

## Clean build required

**Symptom:**

Inexplicable linker errors after a major dependency change.

**Fix:**

```bash
# inside container
cargo clean
cargo build --workspace
```

The target volume is preserved but its contents are cleared. First
post-clean build takes 3–5 minutes.

**Nuclear option** — also clear the cargo registry:

```bash
docker volume rm brain-cargo-registry brain-cargo-git brain-target-cache
just docker-rebuild
```

## Server starts but `/healthz` hangs

**Symptom:**

`curl http://127.0.0.1:9091/healthz` doesn't return within a few
seconds; the server logs `admin server bound` but not
`admin server accepting`.

**Likely cause:**

The shard executors are still spinning up (each pins a CPU core).
On constrained Docker (e.g. macOS with low CPU allocation), this
can take 10+ seconds for a 4-shard config.

**Verify:**

Watch the logs:

```bash
cargo run --bin brain-server -- --config config/dev.toml 2>&1 \
  | jq '.fields.message'
```

The `"listening"` line appears once all shards are ready.

**Mitigation** — reduce shard count for dev:

```bash
BRAIN__STORAGE__SHARD_COUNT=1 \
  cargo run --bin brain-server -- --config config/dev.toml
```

## RECALL returns no results

**Symptom:**

ENCODE returns a non-zero `memory_id`, but RECALL with the same
text returns an empty Vec.

**Possible causes:**

1. **HNSW cold start.** The first ENCODE batch triggers HNSW
   index construction. RECALL within the first ~5 seconds may
   miss recent inserts.
   - **Fix:** wait a few seconds; retry.

2. **Wrong shard.** RECALL queries are routed per agent_id. If
   your test client uses a different agent_id from the one that
   ran ENCODE, you'll hit a different shard.
   - **Verify:** `just cli --output json debug-snapshot --shard 0`
     vs `--shard 1` to see where memories landed.

3. **Tombstoned.** Soft-FORGET hides memories from RECALL.
   - **Verify:** `brain_hnsw_tombstone_count{shard="0"}` — non-zero
     means some tombstones exist.

## ENCODE returns `BadFrame` / `BadOpcode`

**Symptom:**

```
Error frame: code=BadOpcode  category=Protocol  message="unknown opcode"
```

Indicates a wire-protocol mismatch — usually the SDK and server
were built from different commits.

**Fix:**

Rebuild both with `cargo clean && cargo build --workspace`.

**Verify:**

The HELLO/WELCOME handshake should succeed cleanly. Check the
server log for `"handshake complete"` after the SDK connects.

## "permission denied" on the data directory

**Symptom:**

```
Error: ArenaOpenError: permission denied on /workspaces/brain/data
```

The container's user can't write to the configured `data_dir`.

**Fix:**

```bash
sudo chown -R $(id -u):$(id -g) data/
```

Or `rm -rf ./data` and let the server recreate it.

## docker-verify exit code 137

**Symptom:**

```
error: Recipe `docker-verify` failed on line 116 with exit code 137
```

Exit 137 = OOM killed by the container runtime. The full
workspace test suite + clippy can exceed Docker Desktop's default
memory allocation.

**Fix:**

Increase Docker Desktop's RAM allocation to ≥ 6 GiB (Settings →
Resources → Memory). Or run subsets:

```bash
just docker-test -p brain-server
just docker-clippy
```

## Next

[`10-tests.md`](10-tests.md) — running the e2e test suites and
benchmarks.
