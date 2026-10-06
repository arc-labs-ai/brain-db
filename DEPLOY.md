# Deploying Brain

Brain ships as **one immutable, multi-arch container image** on GHCR. You never
clone this repo to run Brain — you pull a released image and point an SDK at it.
The same image is what powers our own hosted launch and the playground, so what
you run is exactly what we run.

- **Image:** `ghcr.io/arc-labs-ai/brain`
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
- **An LLM provider key.** Brain refuses to boot without one: write-time HyPE
  and extraction are always-on and depend on the LLM. The provider is derived
  from the model id (`gpt-4o-mini` → OpenAI, `claude-haiku-4-5` → Anthropic).
- **`seccomp=unconfined`** (or a profile permitting `io_uring_*`). Brain's
  shards use io_uring via Glommio; the default Docker seccomp profile blocks the
  syscalls.
- **Linux host.** Brain is Linux-only (io_uring). It runs fine in Docker on
  macOS/Windows via the Linux VM.

---

## Quickest start — `docker run`

```bash
docker run -d --name brain \
  --security-opt seccomp=unconfined \
  -p 8080:8080 -p 9091:9091 \
  -e BRAIN__LLM__API_KEY=sk-... \
  -e BRAIN__LLM__MODEL=gpt-4o-mini \
  -e BRAIN__ADMIN__TOKEN="$(openssl rand -hex 32)" \
  -v brain-data:/var/lib/brain/data \
  -v brain-models:/var/lib/brain/models \
  ghcr.io/arc-labs-ai/brain:latest

# Liveness:
curl -fsS http://localhost:9091/healthz
```

The embedding model (BGE-small, 384-dim) downloads from HuggingFace on first
boot and is cached in the `brain-models` volume, so restarts don't re-download.
First boot therefore needs egress; later boots don't.

## Recommended — `docker compose`

```bash
curl -fsSLO https://raw.githubusercontent.com/arc-labs-ai/brain-db/main/docker-compose.yml
curl -fsSLO https://raw.githubusercontent.com/arc-labs-ai/brain-db/main/.env.example
cp .env.example .env          # then set BRAIN__LLM__API_KEY + BRAIN__ADMIN__TOKEN
docker compose up -d
docker compose ps             # HEALTHCHECK status shows here
docker compose logs -f brain
```

Pin a version in `.env`: `BRAIN_VERSION=0.1.0`.

---

## Ports & surfaces

| Port | Surface | Exposed? |
|---|---|---|
| **8080** | Data plane — binary wire protocol (CBOR). SDKs connect here. | yes (`-p 8080`) |
| **9091** | Public HTTP — `/healthz`, `/metrics` only. | yes (`-p 9091`) |
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

1. **Single fields** — set `BRAIN__SECTION__FIELD` env vars. This is the only
   env-override mechanism (no bespoke `BRAIN_X` vars). Examples:

   | Env var | Effect |
   |---|---|
   | `BRAIN__LLM__API_KEY` | provider key (**required**) |
   | `BRAIN__LLM__MODEL` | model id → derives provider (default `gpt-4o-mini`) |
   | `BRAIN__ADMIN__TOKEN` | operator admin secret (**required**) |
   | `BRAIN__STORAGE__SHARD_COUNT` | shards; one shard pins one core |
   | `BRAIN__SHARD__ARENA_CAPACITY_BYTES` | per-shard vector arena (e.g. `4GiB`) |
   | `BRAIN__RERANK__ENABLED` | load the cross-encoder reranker (default off) |
   | `BRAIN__MONITORING__TRACING__ENABLED` + `__ENDPOINT` | OTLP tracing |

2. **Whole file** — bind-mount your own:
   `-v /host/brain.toml:/etc/brain/config.toml:ro`.

### Sizing

Defaults are a single-shard, single-container starting point: 1 shard, a 1 GiB
arena (≈660K vectors at 1600 B/slot). For higher write throughput raise
`BRAIN__STORAGE__SHARD_COUNT` toward your core count (each shard pins one core)
and grow the arena to fit your corpus.

---

## Upgrade & rollback

Roll by tag — the image is immutable, so a version is a version:

```bash
# Upgrade
sed -i 's/^BRAIN_VERSION=.*/BRAIN_VERSION=0.2.0/' .env
docker compose pull && docker compose up -d

# Rollback — just pin the old tag and re-up
sed -i 's/^BRAIN_VERSION=.*/BRAIN_VERSION=0.1.0/' .env
docker compose pull && docker compose up -d
```

Data (`brain-data` volume) and the model cache (`brain-models`) survive across
upgrades. On restart Brain replays its WAL and recovers to the last fsynced
write (mechanism: [`spec/18_failure_recovery/`](spec/18_failure_recovery/00_purpose.md)).
Read the release notes before crossing a minor — pre-1.0, breaking changes to the
redb layout / wire protocol are made in place without shims.

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
