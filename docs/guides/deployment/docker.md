# Deploy Brain with Docker (single container)

**Audience:** operators putting Brain into production as a single
Docker container — small team deployments, single-host services,
or as the building block under a multi-host orchestration system.

**Goal:** A correctly-configured, restart-safe, observable Brain
container.

If you've never run Brain before, do the
[5-minute quickstart](../../tutorials/01-quickstart-docker.md)
first. This page assumes you've already seen it work.

---

## The shape

A production Brain container has six things to get right:

1. **Image source.** Build locally vs. pull a tagged release.
2. **Data persistence.** WAL + arena + redb live on disk; lose
   them, lose your memories.
3. **Networking.** Three ports, each with a different policy.
4. **Configuration.** Per-environment overrides without rebuilding
   the image.
5. **Restart + healthcheck.** Brain should come back up by itself.
6. **Log handling.** JSON to stdout → wherever your log pipeline
   collects it.

The rest of this page walks each in turn.

---

## 1. Image source

Brain doesn't yet publish pre-built images to a registry (planned
for the v1.0.0 release). Until then: build locally.

```bash
DOCKER_BUILDKIT=1 docker build -t brain:latest .
```

The build uses BuildKit cache mounts. First build ≈ 15 min;
incremental rebuilds (source changes only) finish in 30-60 s.

To tag for promotion through environments:

```bash
DOCKER_BUILDKIT=1 docker build -t brain:v1.0.0-rc1 .
docker tag brain:v1.0.0-rc1 registry.example/brain:v1.0.0-rc1
docker push registry.example/brain:v1.0.0-rc1
```

The Dockerfile is multi-stage:
- **Builder:** `rust:1-bookworm` — compiles the workspace.
- **Runtime:** `debian:bookworm-slim` — non-root `brain` user
  (UID 10001), `tini` as PID 1, just the two binaries +
  ca-certificates + curl.

Image size: ≈ 350 MB (most of it is libstdc++ / glibc; the Brain
binaries are ≈ 80 MB combined).

---

## 2. Data persistence

Brain writes durable state to `data_dir` (default
`/var/lib/brain/data` inside the container). **Mount a volume on
that path** or you lose state on every restart.

### Named volume (simplest)

```bash
docker run -d --name brain \
    -v brain-data:/var/lib/brain/data \
    -v brain-models:/var/lib/brain/models \
    brain:latest
```

Pros: Docker manages the lifecycle. Cons: harder to back up
ad-hoc (you go through the docker volume plugin).

### Host bind mount (best for production)

```bash
mkdir -p /srv/brain/{data,models}
chown -R 10001:10001 /srv/brain          # match the in-container `brain` user

docker run -d --name brain \
    -v /srv/brain/data:/var/lib/brain/data \
    -v /srv/brain/models:/var/lib/brain/models \
    brain:latest
```

The UID 10001 chown is required — Brain runs as a non-root user
and can't write to a host-mounted directory owned by root.

### Two volumes? Why?

- **`data`** is hot (WAL, arena, redb). Lives on your fastest
  disk. Backed up per [`backup-restore.md`](backup-restore.md).
- **`models`** holds the BGE-small embedding model (~130 MB),
  downloaded from HuggingFace on first start. Cold; can be
  shared between deployments; doesn't need backup.

### Sizing

| Workload | `data` volume size |
|---|---|
| Dev / smoke | 2 GiB |
| ~100 K memories | 8 GiB |
| ~1 M memories | 40 GiB |
| ~10 M memories | 400 GiB |

These are rough — the actual per-memory cost depends on text
length, metadata, edges, and WAL retention. See
[`../tuning/shard-sizing.md`](../tuning/shard-sizing.md) before
committing to a large allocation.

---

## 3. Networking

Brain binds three ports, each with a different exposure policy.

| Port | Bound to | Purpose | Public? |
|---|---|---|---|
| 8080 | `0.0.0.0` | Data plane (rkyv wire protocol) | **No** — never. |
| 9091 | `0.0.0.0` | HTTP — `/healthz`, `/metrics`, `/v1/*` | Yes, *if* fronted by auth |
| 9090 | `127.0.0.1` *(inside container)* | Admin CLI surface | No — loopback only |

### The rule

Until token/mTLS auth ships (Phase 14+), **port 8080 must not be
reachable from the public internet**. Either:

- Bind to a private interface: `-p 10.0.0.5:8080:8080`
- Route through a reverse proxy (nginx, Envoy, Caddy) that
  enforces auth and rate-limits.
- Sit behind a service mesh that enforces mTLS at the boundary.

See [`../security/network.md`](../security/network.md) for
patterns.

### Standard `docker run`

```bash
docker run -d --name brain \
    --restart unless-stopped \
    -p 8080:8080 \
    -p 9091:9091 \
    -v /srv/brain/data:/var/lib/brain/data \
    -v /srv/brain/models:/var/lib/brain/models \
    brain:latest
```

The admin port isn't `-p`'d — that's deliberate. To talk to it:

```bash
docker exec brain brain-cli health
docker exec brain brain-cli stats
docker exec brain brain-cli worker list
```

---

## 4. Configuration

The image bakes [`config/docker.toml`](../../../config/docker.toml)
at `/etc/brain/config.toml`. Override either:

### Per-field via env

```bash
docker run -d --name brain \
    -e BRAIN__STORAGE__SHARD_COUNT=4 \
    -e BRAIN__SHARD__ARENA_CAPACITY_BYTES=4GiB \
    -e BRAIN__TRACING__ENABLED=true \
    -e BRAIN__TRACING__ENDPOINT=http://otel.example:4318/v1/traces \
    ... \
    brain:latest
```

Pattern: `BRAIN__SECTION__FIELD` — double underscores separate
nesting. Every TOML field in
[`../../reference/configuration.md`](../../reference/configuration.md)
is overridable this way.

### Whole-file via bind mount

```bash
docker run -d --name brain \
    -v /etc/brain/prod.toml:/etc/brain/config.toml:ro \
    ... \
    brain:latest
```

For multi-environment deployments (dev / staging / prod with
materially different topologies), bind-mounting per-env files is
cleaner than threading 30 env vars through your orchestrator.

### Validating config

Brain validates at startup; bad config aborts before binding any
port:

```
config error: invalid socket address `not-an-addr`
```

Dry-run validation:

```bash
docker run --rm \
    -v /etc/brain/prod.toml:/etc/brain/config.toml:ro \
    brain:latest --config /etc/brain/config.toml --version
```

(The `--version` exit path still loads + validates the file
before printing the version and exiting.)

---

## 5. Restart + healthcheck

The Dockerfile ships a `HEALTHCHECK` hitting `/healthz` on the
metrics port. Pair it with a restart policy:

```bash
--restart unless-stopped
--health-interval=10s --health-timeout=3s --health-start-period=30s --health-retries=3
```

(The image already has these `HEALTHCHECK` values baked in; pass
them on `docker run` only if you want to override.)

### What "healthy" means

`/healthz` returns `ok` only when:
- The HTTP server is listening.
- Every shard's WAL is open and writable.
- The embedder model is loaded.

It does **not** check upstream LLM connectivity (for the
summarizer worker) or trace-collector reachability. Those degrade
gracefully — Brain stays healthy and surfaces them via metrics.

### Stopping cleanly

```bash
docker stop brain
```

`tini` propagates SIGTERM to `brain-server`, which:
1. Stops accepting new connections.
2. Drains in-flight requests up to a 10 s deadline.
3. Stops the workers in dependency order.
4. Flushes WAL.
5. Exits cleanly.

If you need a hard kill (don't, normally):

```bash
docker kill brain                  # SIGKILL — risks partial WAL write
```

Brain recovers from an uncommitted partial WAL record on next
start (it's CRC-checked and discarded), but you might lose the
last few unacknowledged writes. Always prefer `docker stop`.

---

## 6. Log handling

Brain logs JSON to stdout. The image default is
`format = "json"`, which Loki / Vector / Fluent Bit / CloudWatch /
Datadog all parse natively.

### Stdout to a host file (simplest)

```bash
docker run ... --log-driver json-file --log-opt max-size=100m --log-opt max-file=5 brain:latest
docker logs -f brain
```

### Stdout to a sidecar

```bash
docker run ... --log-driver fluentd --log-opt fluentd-address=localhost:24224 brain:latest
```

### Levels

Override at startup:

```bash
-e BRAIN__LOGGING__LEVEL=debug
```

Don't run `debug` in production — Brain emits one structured log
line per RPC at that level.

---

## Offline installs

Disconnected hosts can't download the BGE-small model on first
start. Pre-populate the `models` volume on a connected host:

```bash
# On a connected host:
docker run --rm -v brain-models:/var/lib/brain/models brain:latest \
    --warm-model
# (--warm-model exits after the model is on disk; spec §13/03.)

# Then export and transfer:
docker run --rm -v brain-models:/srv:ro alpine \
    tar -czf - -C /srv . > brain-models.tar.gz
```

On the offline host:

```bash
docker volume create brain-models
docker run --rm -v brain-models:/dst -v "$(pwd)":/src alpine \
    sh -c 'cd /dst && tar -xzf /src/brain-models.tar.gz'
```

---

## What's next

- Add Prometheus + Grafana + OTel collector → [`docker-compose.md`](docker-compose.md).
- Terminate TLS on the data port → [`tls.md`](tls.md).
- Take backups → [`backup-restore.md`](backup-restore.md).
- Tune for your workload → [`../tuning/`](../tuning/).
- Wire observability into existing stack → [`../observability.md`](../observability.md).

## See also

- [`../../reference/configuration.md`](../../reference/configuration.md)
  — every config field, every default.
- [`../../runbooks/substrate-down.md`](../../runbooks/substrate-down.md)
  — runbook for "Brain won't start".
- [`../../../Dockerfile`](../../../Dockerfile) — the source.
