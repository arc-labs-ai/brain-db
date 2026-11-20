# Deploying Brain

Brain ships as **one immutable, multi-arch container image** on GHCR. You never
clone this repo to run Brain — you pull a released image and point an SDK at it.
The same image is what powers our own hosted launch and the playground, so what
you run is exactly what we run.

- **Image:** `ghcr.io/arc-labs-ai/brain-db`
- **Arches:** `linux/amd64`, `linux/arm64`
- **Tags:** `X.Y.Z` (immutable release), `X.Y` (latest patch of a minor),
  `latest` (latest release). Pin `X.Y.Z` — or a digest — in production.
- **SDKs** (talk to Brain without touching the wire protocol):
  `cargo add brain-db-sdk` · `npm i @brain-db/sdk` · `pip install brain-db-sdk`

> **Scope.** This is the *deployment on-ramp* — getting the container running and a
> client connected. It is **not** the operational reference: the authoritative
> runbooks (GC / WAL-retention / slot-reclaim triggers, corruption recovery,
> per-shard restart, admin-op semantics) live in
> [`spec/17_observability/`](spec/17_observability/00_purpose.md) and
> [`spec/18_failure_recovery/`](spec/18_failure_recovery/00_purpose.md), and the
> wire/admin surface in [`spec/04_wire_protocol/`](spec/04_wire_protocol/00_purpose.md).
> This file links there rather than restating it.

---

## Prerequisites

- **Docker** (or any OCI runtime / Kubernetes).
- **No LLM credential.** Write-time HyPE and extraction are always-on and
  depend on an LLM, but a deployment holds no provider key: each namespace
  registers its own over the wire and Brain seals it. Nothing gates boot on a
  credential, so `brain-db` starts standalone. You do **not** need a provider
  key of your own — but each namespace needs one before it can write: HyPE and
  extraction both require it, so an ENCODE for a namespace with no registered
  key is refused with `LlmCredentialMissing`.
- **`seccomp=unconfined`** (or a profile permitting `io_uring_*`). Brain's
  shards use io_uring via Glommio; the default Docker seccomp profile blocks the
  syscalls.
- **`memlock` unlimited.** io_uring pins its submission and completion rings,
  so the memlock rlimit has to be raised: `--ulimit memlock=-1` on `docker run`,
  or `ulimits: memlock: -1` in compose. Both this and `seccomp` are needed; one
  without the other still fails to bring a shard up.
- **Linux host.** Brain is Linux-only (io_uring). It runs fine in Docker on
  macOS/Windows via the Linux VM.

---

## Quickest start — `docker run`

```bash
docker run -d --name brain \
  --security-opt seccomp=unconfined --ulimit memlock=-1 \
  -p 8080:8080 -p 9091:9091 \
  -e BRAIN__LLM__MODEL=gpt-4o-mini \
  -e BRAIN__ADMIN__TOKEN="$(openssl rand -hex 32)" \
  -v brain-data:/var/lib/brain/data \
  ghcr.io/arc-labs-ai/brain-db:latest

# Liveness:
curl -fsS http://localhost:9091/healthz
```

The embedding model (BGE-small-en-v1.5, 384-dim) **ships inside the image** at
`/opt/brain/models/bge-small-en-v1.5` — no volume, no download, no egress at
any boot. brain-server has no download path and cannot start without a model on
disk, so bundling it is what makes `docker run` a single command. It accounts
for ~130 MB of the image.

To run a different BERT-shaped model, mount it and point the server at it:

```bash
-v /path/to/my-model:/models/my-model \
-e BRAIN_EMBED_MODEL_DIR=/models/my-model
```

That variable is the first branch of the server's model-path resolution, so it
wins over everything else. The directory must contain `config.json`,
`tokenizer.json` and `model.safetensors`.

Changing the embedding model changes the embedding fingerprint; vectors written
under one model are not comparable with another. Re-embed rather than mixing.

### Cross-encoder rerank is not available in this release

`[rerank]` ships `enabled = false` and the image carries no cross-encoder model.
Opt-in rerank requests return `CapabilityNotEnabled`, and
the `GET_CAPABILITIES` wire op reports `rerank: false`. Retrieval fuses with RRF.

The GLiNER NER model (`gliner-small-v2.1`) also ships in the image, at
`/opt/brain/models/gliner-small-v2.1`. It is the classifier tier of the
extractor pipeline — the tier that fixes entity span boundaries — and it is
**required**: like the embedder, Brain refuses to start without it.

A Brain that starts is a Brain that extracts at full strength: starting with the
tier unloaded would mean extracting more weakly on every write while still
reporting healthy, with nothing but one boot log line to distinguish the two.
The embedder fails stop for the same reason.

Together the two models account for roughly 720 MB of the image. To point the
classifier at your own GLiNER build, override the config field:

```bash
-v /path/to/my-gliner:/models/my-gliner \
-e BRAIN__EXTRACTORS__CLASSIFIER__MODEL_PATH=/models/my-gliner
```

That directory needs `pytorch_model.bin`, `gliner_config.json`,
`tokenizer.json` and `config.json`, and its `tokenizer.json` must carry
`<<ENT>>` at id 128001 and `<<SEP>>` at 128002 — the loader resolves those by
name and refuses to load without them. See
[`crates/brain-extractors/docs/bundled-ner.md`](crates/brain-extractors/docs/bundled-ner.md).

## Recommended — `docker compose`

```bash
curl -fsSLO https://raw.githubusercontent.com/arc-labs-ai/brain-db/main/docker-compose.yml
curl -fsSLO https://raw.githubusercontent.com/arc-labs-ai/brain-db/main/.env.example
cp .env.example .env          # then set the one required value (see below)
docker compose up -d
docker compose ps             # HEALTHCHECK status shows here
docker compose logs -f brain
```

One value is required and `docker compose up` refuses to start without it:
`BRAIN__ADMIN__TOKEN`. There is no provider key to set — each namespace
registers its own through `LLM_CREDENTIAL_PUT` once the server is up.

Pin a version in `.env`: `BRAIN_VERSION=0.4.0`.

---

## Ports & surfaces

| Port | Surface | Exposed? |
|---|---|---|
| **8080** | Data plane — binary wire protocol (CBOR). SDKs connect here. | yes (`-p 8080`) |
| **9091** | Public HTTP — `/healthz` (liveness), `/readyz` (200 once every shard serves, 503 otherwise), `/metrics`. No auth, no data. | yes (`-p 9091`) |
| **9092** | Admin HTTP — `/v1/*` (API-key mint/revoke, worker control, audit, snapshots, per-shard status). Stats/metrics live on the public `:9091/metrics`, not here. | **no** — loopback inside the container |

The admin plane has no built-in auth beyond the operator token — it stays on
container loopback by design. Reach it via `docker compose exec`:

```bash
docker compose exec brain \
  curl -fsS -H "Authorization: Bearer $BRAIN__ADMIN__TOKEN" \
  http://127.0.0.1:9092/v1/workers
```

To front it externally, put it behind your own token/mTLS proxy — never map
`-p 9092` to a public interface. Full admin-op catalog + semantics:
[`spec/17_observability/04_admin_ops.md`](spec/17_observability/04_admin_ops.md).

---

## Configuration

The image bakes defaults at `/etc/brain/config.toml`. Two override paths:

1. **Single fields** — set `BRAIN__SECTION__FIELD` env vars. Double underscore
   separates nesting; each component is lowercased to match the TOML key. Any
   field in the file is reachable this way. Examples:

   | Env var | Effect |
   |---|---|
   | `BRAIN__LLM__CREDENTIALS__KEK` | the key that seals stored credentials, 64 hex chars. Unset, Brain generates one into its data directory on first boot |
   | `BRAIN__LLM__MODEL` | fallback model id when a registered credential names none. The server has no built-in default; `docker-compose.yml` and `.env.example` set `gpt-4o-mini` |
   | `BRAIN__ADMIN__TOKEN` | operator admin secret (**required** — the admin listener refuses to start without it) |
   | `BRAIN__STORAGE__SHARD_COUNT` | shards; one shard pins one core |
   | `BRAIN__SHARD__ARENA_CAPACITY_BYTES` | per-shard vector arena (e.g. `4GiB`) |
   | `BRAIN__RERANK__ENABLED` | load the cross-encoder reranker (default off — **fatal at boot on the stock image**, which carries no model) |
   | `BRAIN__MONITORING__TRACING__ENABLED` + `__ENDPOINT` | OTLP tracing |

   Two variables sit outside the `BRAIN__` scheme, because they are read before
   or independently of the config tree:

   | Env var | Effect |
   |---|---|
   | `BRAIN_EMBED_MODEL_DIR` | absolute path to the embedder, first branch of the model-path cascade (shown above) |
   | `BRAIN_LOG` | tracing filter, overriding `[monitoring.logging] level`. `RUST_LOG` is deliberately ignored so there is one way to set the level |

   Everything else goes through `BRAIN__SECTION__FIELD` or the file. **Every
   config struct is `#[serde(deny_unknown_fields)]`**, so a misspelled or
   invented key is a startup parse error, not a warning — including one set
   through the env scheme.

2. **Whole file** — bind-mount your own:
   `-v /host/brain.toml:/etc/brain/config.toml:ro`.

### Sizing

The in-image defaults are 4 shards and a 1 GiB arena **per shard** (≈660K
vectors each, at 1600 B/slot — a 1536-byte vector plus 64 bytes of metadata).
Each shard pins one core, so 4 shards want at least 5 available: one core is
held back for the network and admin runtimes, which do not run on a shard
executor.

Raise `BRAIN__STORAGE__SHARD_COUNT` toward your core count for write
throughput, and grow `BRAIN__SHARD__ARENA_CAPACITY_BYTES` to fit your corpus.
Unset `shard_count` entirely and the server sizes itself to the machine —
`available_parallelism()` minus one, which reads the cgroup quota, so a
container limited to two CPUs gets one shard rather than the host's core count.

**Pick the count before the first boot.** A `MemoryId` encodes its shard, so
the count the data directory was created with is recorded in `layout.json` and
checked on every open: starting with a different count is refused with the
recorded value in the error, rather than silently routing every existing id at
a shard that does not hold it. There is no reshard tool.

`[analytics]` (off by default) adds, per shard, `threads` OS threads and up to
`memory_limit_mb / shard_count` of scan memory; exports add ~150 MiB per thread
while running, capped at `max_concurrent_exports` exports across the server.

---

## Upgrade & rollback

Roll by tag — the image is immutable, so a version is a version:

```bash
# Upgrade
sed -i 's/^BRAIN_VERSION=.*/BRAIN_VERSION=0.4.0/' .env
docker compose pull && docker compose up -d

# Rollback — just pin the old tag and re-up
sed -i 's/^BRAIN_VERSION=.*/BRAIN_VERSION=0.3.0/' .env
docker compose pull && docker compose up -d
```

Data (the `brain-data` volume) survives across upgrades; the embedding model
rides in the image, so it needs no volume and is replaced with the image. On restart Brain replays its WAL and recovers to the last fsynced
write (mechanism: [`spec/18_failure_recovery/`](spec/18_failure_recovery/00_purpose.md)).
Read the release notes before crossing a minor — pre-1.0, breaking changes to the
redb layout / wire protocol are made in place without shims.

**Aggregate counters after an upgrade.** The first start on data written by a
release without them builds the `MEMORY_AGGREGATE` counters in the background
(2 000 memories per 20 ms tick per shard; no startup delay). Until a shard
finishes, `MEMORY_AGGREGATE` answers `RetrievalUnavailable` and
`GET /v1/aggregates` answers `503`; watch `brain_agg_counters_ready`. Rolling
back to a release without counters and forward again is safe: the periodic
verify (first one 5 minutes after start) finds and repairs any counters the
older release left stale, and reports it in
`brain_agg_counter_drifted_buckets_total`.

**Schema v2 (salience decays when read).** This release stores each memory's
salience as of an anchor time instead of a value a decay worker rewrites (the
decay worker and `[workers] decay_interval_sec` are gone; remove that key from
configs).
The redb layout changed, so a v1 data directory (0.4.3) is refused at open
(`SchemaTooOld`); as with earlier pre-1.0 bumps there is no migration tool —
start from a fresh data volume. Salience values change too: they are now
exact at read time everywhere, where before the hourly decay batches left
most memories in large shards decayed far less than their age implies.

**Analytics disk use.** With `[analytics] enabled = true`, each shard keeps up to
two snapshots of its memory metadata under `<data>/<shard>/analytics/` — each
16 Parquet files bucketed by `(namespace, space)`, ~52 bytes per memory — plus up
to two deltas and up to `spill_limit_mb` of scan spill files. Each export
interval writes only a delta of the memories changed since the snapshot; once
that passes 10% of it, the delta is compacted into a new snapshot from the
Parquet files alone (no database scan). To know what changed, the shard logs
`(sequence, memory id, bucket)` for every write while analytics is on (a few
dozen bytes per write until the next compaction, then pruned); with analytics
off nothing is logged and any old log is dropped at start.
Only the first export (and one after change tracking was off) scans the
database; it and the daily counter verify read the metadata
store inside one long read transaction (seconds to minutes per shard). While
it runs, redb cannot reuse pages freed by concurrent writes, so `metadata.redb`
can grow by roughly the write volume during that window; the space is reused
afterwards. Size the data volume for one export interval's worth of writes.

---

## Backup

Everything durable lives in the `brain-data` volume (WAL, arena, redb
metadata). Snapshot it while the container is stopped for a consistent copy:

```bash
docker compose stop brain
docker run --rm -v brain-data:/data -v "$PWD":/backup alpine \
  tar czf /backup/brain-data-$(date +%F).tar.gz -C /data .
docker compose start brain
```

For hot backups, use the volume driver's snapshot facility (LVM/ZFS/EBS) rather
than copying files from under a running writer, or Brain's own HTTP snapshot +
restore endpoints (`/v1/snapshots`) — see
[`spec/17_observability/04_admin_ops.md`](spec/17_observability/04_admin_ops.md).

---

## Connecting a client

Install an SDK and point it at the data-plane port. Every request authenticates
with an API key you mint via the admin plane:

```bash
# Mint a key (admin plane, over container loopback). `space_id_hex` is the
# 16-byte space id as 32 hex chars; `namespace` is the schema namespace the
# key writes into; `permissions` is a named-list or a raw u32 bitfield.
docker compose exec brain \
  curl -fsS -X POST -H "Authorization: Bearer $BRAIN__ADMIN__TOKEN" \
  -H 'content-type: application/json' \
  -d '{
        "space_id_hex": "00000000000000000000000000000001",
        "namespace": "acme",
        "permissions": ["READ_WRITE"]
      }' \
  http://127.0.0.1:9092/v1/api-keys
```

```python
# pip install brain-db-sdk  — illustrative; the SDK repo README is authoritative
from brain_db_sdk import BrainClient
client = BrainClient("localhost:8080", api_key="brn_...")
client.encode("Ada prefers dark roast coffee.")
print(client.recall("what coffee does Ada like?"))
```

The exact client constructor / method surface is documented in the SDK repo
([`arc-labs-ai/brain-sdk`](https://github.com/arc-labs-ai/brain-sdk)); the wire
protocol is in `spec/04_wire_protocol/` for anyone building a client directly.
The SDKs are the supported path.
