# 03 — Run the server

Start `brain-server` against the dev config and confirm it's
serving on the three local ports.

## 1. Start the server (dev config)

The dev config is `config/dev.toml`. It binds three ports on
localhost:

| Port | Purpose | Used by |
|---|---|---|
| 9090 | Data plane (wire protocol, TCP) | SDK clients, agents |
| 9091 | Admin + Prometheus metrics (HTTP) | `brain` CLI, Prometheus |
| 9092 | Admin HTTP (additional admin endpoint) | `brain` CLI for health / config |

**Input:**

```bash
cargo run --bin brain-server -- --config config/dev.toml
```

Or via the Justfile shortcut:

```bash
just run-server
```

**Expected startup log** (JSON format per `dev.toml`):

```json
{"timestamp":"2026-05-15T12:00:00.123Z","level":"INFO","fields":{"message":"brain-server starting","version":"0.1.0","shards":4},"target":"brain_server"}
{"timestamp":"...","level":"INFO","fields":{"message":"admin server bound","addr":"127.0.0.1:9091"},"target":"brain_server::admin"}
{"timestamp":"...","level":"INFO","fields":{"message":"admin server accepting","addr":"127.0.0.1:9091"},"target":"brain_server::admin"}
{"timestamp":"...","level":"INFO","fields":{"message":"listening","listen":"127.0.0.1:9090","metrics":"127.0.0.1:9091","admin":"127.0.0.1:9092","shards":4,"data_dir":"./data"},"target":"brain_server"}
```

The server is ready when the **`"listening"`** line appears.

**Verify (from a second terminal in the container, or from host
via `just docker`):**

```bash
curl -s http://127.0.0.1:9091/healthz
```

Expected:

```
ok
```

```bash
curl -s http://127.0.0.1:9091/metrics | grep "^brain_up "
```

Expected:

```
brain_up 1
```

If either fails, see [`09-troubleshooting.md`](09-troubleshooting.md).

## 2. Override config via environment variables

Any TOML field can be overridden with `BRAIN__SECTION__FIELD=value`.
Double underscores separate nesting levels.

**Input:**

```bash
BRAIN__SERVER__LISTEN_ADDR=0.0.0.0:9090 \
BRAIN__STORAGE__SHARD_COUNT=8 \
BRAIN__SHARD__ARENA_CAPACITY_BYTES=2GiB \
cargo run --bin brain-server -- --config config/dev.toml
```

**Verify:**

The `"listening"` log line reflects the overrides — `listen` field
shows `0.0.0.0:9090`, `shards` shows `8`.

Or scrape:

```bash
curl -s http://127.0.0.1:9091/metrics | grep "brain_config_info\|brain_shards_total"
```

The `brain_config_info` line carries `shard_count="8"`,
`arena_capacity_bytes="2147483648"`.

See [`07-configuration.md`](07-configuration.md) for the full env
mapping.

## 3. Data directory

The server writes shard data to `data/` relative to the working
directory by default. Each shard gets its own subdirectory:

```
data/
  shard-0/
    arena.bin
    wal-000001.seg
    metadata.redb
  shard-1/
  ...
```

**Verify after first encode:**

```bash
ls -la data/shard-0/
```

Should show `arena.bin` (sized per `arena_capacity_bytes`), at
least one `*.wal` segment, and `metadata.redb`.

To start fresh:

```bash
rm -rf ./data
```

The server recreates the directory on next start.

## 4. Stop the server

`Ctrl+C` in the terminal running it. Expected shutdown log:

```json
{"timestamp":"...","level":"INFO","fields":{"message":"shutdown signal received"},...}
{"timestamp":"...","level":"INFO","fields":{"message":"admin server shutdown complete"},...}
{"timestamp":"...","level":"INFO","fields":{"message":"shards joined"},...}
```

Clean shutdown finishes WAL flushes before exit; the next start
recovers from the WAL.

**Verify durability across restarts:**

After encoding some memories (see [`05-sdk.md`](05-sdk.md) or
[`06-walkthrough.md`](06-walkthrough.md)), `Ctrl+C` the server and
restart it:

```bash
cargo run --bin brain-server -- --config config/dev.toml
```

Then RECALL the same cues — the memories should still be returned.
WAL records were `fsync`'d before each ENCODE returned; the arena
+ metadata replay on restart.

## Next

[`04-cli.md`](04-cli.md) — every `brain` CLI command with input /
output / verify.
