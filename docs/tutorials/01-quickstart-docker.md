# Quickstart — Brain in Docker

**Audience:** you've never run Brain before. You have Docker
(Docker Desktop, OrbStack, or any Linux engine) installed.

**Goal:** Brain running locally in about five minutes. Health
endpoint returning `ok`. Then a pointer to what comes next.

This is a **tutorial** (learning by doing). Once you've finished
it, the production-shaped how-to is at
[`../guides/deployment/docker.md`](../guides/deployment/docker.md).

---

## 0. Check Docker is alive

```bash
docker info >/dev/null && echo "docker ok"
```

If that prints `docker ok`, you're set. If not, start Docker Desktop / OrbStack.

> **macOS / Windows note.** Brain needs a Linux kernel (≥ 5.15)
> with `io_uring` available. Docker Desktop and OrbStack both
> run Brain inside their built-in Linux VM — that VM already
> satisfies this. You don't need to do anything special.

## 1. Build the image

From the repository root:

```bash
DOCKER_BUILDKIT=1 docker build -t brain:latest .
```

First build takes ~15 minutes (Brain is a sizeable Rust workspace
with `candle` and friends). Subsequent builds reuse cargo's
registry + target cache and finish in seconds for source-only
changes.

## 2. Start the container

```bash
docker run --rm --name brain \
    -p 8080:8080 -p 9091:9091 \
    -v brain-data:/var/lib/brain/data \
    -v brain-models:/var/lib/brain/models \
    brain:latest
```

That foregrounds the container. You'll see Brain's startup logs:

```
{"level":"info","msg":"brain-server starting","version":"...","listen":"0.0.0.0:8080","metrics":"0.0.0.0:9091","admin":"127.0.0.1:9090","shards":1,"data_dir":"/var/lib/brain/data"}
```

Open a second terminal for the next steps.

## 3. Probe the health endpoint

```bash
curl -fsS http://127.0.0.1:9091/healthz
# → ok
```

If you get `ok`, Brain is up. If `curl` exits non-zero, jump to
**Troubleshooting** below.

## 4. Look at the metrics

```bash
curl -s http://127.0.0.1:9091/metrics | head -20
```

You should see Prometheus-style lines starting with `brain_`.
These are the substrate-level counters Brain emits — request
rates, WAL flush latency, HNSW depth, worker run counts, etc.
Full catalog: [`../reference/metrics.md`](../reference/metrics.md).

## 5. Use the admin CLI

The CLI ships inside the image. `docker exec` it:

```bash
docker exec brain brain-cli health
```

```
status: ok
uptime_sec: 12
shard_count: 1
```

And:

```bash
docker exec brain brain-cli stats
```

You'll see encode/recall counters, all zero so far — you haven't
written anything yet.

```bash
docker exec brain brain-cli worker list
```

Lists the twelve background workers (decay, consolidation, HNSW
maintenance, …) and their last-run timestamps.

> **Why `docker exec`?** The admin port (9092) serves the
> `/v1/*` routes (worker list, snapshots, audit, …) and is
> bound to loopback *inside the container* — not exposed to
> your host. That's the production-recommended posture: v1 has
> no built-in admin auth, so nothing outside the container
> should reach the admin surface. The CLI runs *inside* the
> container via `docker exec` and talks to it over loopback.
> The public HTTP listener on 9091 (`/healthz` + `/metrics`)
> is the only HTTP surface published to the host.

## 6. Stop the container

Back in the foregrounded terminal: **Ctrl-C**. Brain logs:

```
{"level":"info","msg":"brain-server shutting down","reason":"signal"}
```

The data volume (`brain-data`) persists between runs — start the
container again with `docker run --rm --name brain -p 8080:8080 -p 9091:9091 -v brain-data:/var/lib/brain/data -v brain-models:/var/lib/brain/models brain:latest` and your state is back.

To wipe everything:

```bash
docker volume rm brain-data brain-models
```

---

## What you just did

- Built the production Brain image.
- Ran a single-shard substrate.
- Confirmed health, metrics, and CLI access.

You did **not** yet:

- ENCODE or RECALL any memories.
- Declare a schema (knowledge layer).
- Wire monitoring (Prometheus + Grafana).

Pick the next page based on where you're heading:

| Next step | Page |
|---|---|
| Store memories and run hybrid queries from Rust | [`02-first-substrate-app.md`](02-first-substrate-app.md) |
| Switch on the knowledge layer (typed statements) | [`03-first-knowledge-app.md`](03-first-knowledge-app.md) |
| Add Prometheus + Grafana to the stack | [`../guides/deployment/docker-compose.md`](../guides/deployment/docker-compose.md) |
| Take this beyond a toy (bind mounts, restart policy, log driver) | [`../guides/deployment/docker.md`](../guides/deployment/docker.md) |

---

## Troubleshooting

### `curl: Connection refused`

The container isn't listening yet, or it crashed. Check:

```bash
docker ps                 # is `brain` running?
docker logs brain         # what did it say?
```

Common causes:
- Image is still building (step 1 hasn't finished).
- Port 9091 is occupied on your host. Stop the conflict, or
  remap with `-p 19091:9091` and adjust the curl URL.

### `Healthcheck status: starting` forever

Brain validates config + opens WAL + initialises shards before
binding the HTTP port. On a fast host that takes ~2 seconds; on a
slow host (cold disk, first-time WAL allocation) it can take ~15.
The container's `HEALTHCHECK` has a 30 s start-period — wait it
out, then re-check.

### `Error: text-embedding model not found`

First start downloads the BGE-small model (~130 MB) from
HuggingFace into `/var/lib/brain/models`. The container needs
network to do this. If your runtime blocks egress, pre-populate
the volume — see [`../guides/deployment/docker.md`](../guides/deployment/docker.md#offline-installs).

### Logs show `io_uring_setup: function not implemented`

You're on a Linux kernel older than 5.15. Brain requires
io_uring. Upgrade the kernel, or run on a host whose Docker
engine sits on a newer kernel.
