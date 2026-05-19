# Deploy Brain with docker-compose (full stack)

**Audience:** operators who want Brain plus the observability
stack (Prometheus + Grafana + OTel collector) on a single host
with one command.

**Goal:** A working observability-included Brain deployment in
the time it takes to pull four images.

This page assumes you've read
[`docker.md`](docker.md) (single-container production) or the
[5-minute quickstart](../../tutorials/01-quickstart-docker.md).

---

## The stack

The repo ships [`docker-compose.yml`](../../../docker-compose.yml)
at the root. Four services:

| Service | Image | Purpose | Host ports |
|---|---|---|---|
| `brain` | `brain:latest` (built from `Dockerfile`) | The substrate | 8080, 9091 |
| `prometheus` | `prom/prometheus:v2.55.1` | Scrapes brain on 9091/metrics | 9090 |
| `otel-collector` | `otel/opentelemetry-collector-contrib:0.111.0` | Receives OTLP/HTTP traces | 4318 |
| `grafana` | `grafana/grafana:11.3.1` | Dashboards | 3000 |

All four share the default `brain` network, addressing each other
by service name (e.g. `brain` resolves to the Brain container
from inside `prometheus`).

---

## Bring it up

```bash
docker compose up -d --build
```

`--build` rebuilds the Brain image if its source changed. First
run pulls 3 images (~600 MB total) and builds Brain (~15 min from
cold).

Once up, you'll see:

```
brain         : http://127.0.0.1:9091/healthz
prometheus UI : http://127.0.0.1:9090
grafana       : http://127.0.0.1:3000 (anonymous viewer enabled)
```

Health check:

```bash
curl -fsS http://127.0.0.1:9091/healthz                    # → ok
curl -fsS http://127.0.0.1:9090/-/healthy                  # → Healthy
curl -fsS http://127.0.0.1:13133/                          # → OTel collector OK
```

## What's wired

### Prometheus scrapes Brain

[`deploy/compose/prometheus.yml`](../../../deploy/compose/prometheus.yml)
defines one scrape job pointing at `brain:9091/metrics`. It also
mounts [`monitoring/alerts/`](../../../monitoring/alerts/) as
rule files — Brain's alert taxonomy evaluates in this stack
out-of-the-box (Alertmanager not included; routing is your call).

Check the targets at <http://127.0.0.1:9090/targets>. All three
targets (`brain`, `prometheus`, `otel-collector`) should be `UP`.

### Brain sends traces to the OTel collector

The compose file sets these env vars on the `brain` service:

```yaml
BRAIN__TRACING__ENABLED: "true"
BRAIN__TRACING__ENDPOINT: "http://otel-collector:4318/v1/traces"
BRAIN__TRACING__SAMPLER: "ratio"
BRAIN__TRACING__SAMPLE_RATIO: "0.1"   # 10 % — fine for a single-host stack
```

[`deploy/compose/otel-collector.yaml`](../../../deploy/compose/otel-collector.yaml)
configures the collector to log received spans via the `debug`
exporter. Inspect them with:

```bash
docker compose logs -f otel-collector
```

For a real backend (Tempo, Jaeger, Honeycomb, Datadog, …),
replace the `exporters:` block with the appropriate exporter.
The OTel docs cover every option.

### Grafana auto-provisions the Brain dashboards

[`monitoring/dashboards/`](../../../monitoring/dashboards/) is
mounted into the Grafana container; an auto-provisioning config
([`grafana-dashboards.yaml`](../../../deploy/compose/grafana-dashboards.yaml))
imports every JSON file as a dashboard in the *Brain* folder.

Anonymous viewer login is enabled — point your browser at
<http://127.0.0.1:3000> and the dashboards appear under
*Dashboards → Browse → Brain*. To make changes (or add panels),
log in: `admin` / `brain`.

---

## Bring it down

```bash
docker compose down            # stops everything; volumes preserved
docker compose down -v         # also drops the data volumes (nuclear)
```

---

## Customising

The compose file is a sensible default, not a contract. Common
adjustments:

### Change Brain config

Either bind-mount your own TOML:

```yaml
services:
  brain:
    volumes:
      - brain-data:/var/lib/brain/data
      - brain-models:/var/lib/brain/models
      - ./my-brain.toml:/etc/brain/config.toml:ro     # ADD
```

Or override fields via env:

```yaml
services:
  brain:
    environment:
      BRAIN__STORAGE__SHARD_COUNT: "4"
      BRAIN__SHARD__ARENA_CAPACITY_BYTES: "4GiB"
```

### Add Alertmanager

```yaml
  alertmanager:
    image: prom/alertmanager:v0.27.0
    container_name: brain-alertmanager
    restart: unless-stopped
    ports:
      - "9093:9093"
    volumes:
      - ./alertmanager.yml:/etc/alertmanager/alertmanager.yml:ro
```

Then add to the prometheus service's `command:`:

```yaml
      - --alertmanager.notification-queue-capacity=10000
```

…and Prometheus's config:

```yaml
alerting:
  alertmanagers:
    - static_configs:
        - targets: ["alertmanager:9093"]
```

### Replace the in-stack OTel collector with an upstream backend

If you already have an OTel collector / Tempo / Jaeger reachable:

```yaml
services:
  brain:
    environment:
      BRAIN__TRACING__ENDPOINT: "http://your-collector.example:4318/v1/traces"
```

…and drop the `otel-collector` service from the file.

### Run multiple shards

Bigger workloads: bump `shard_count`. Brain will use one core per
shard:

```yaml
services:
  brain:
    environment:
      BRAIN__STORAGE__SHARD_COUNT: "8"
      BRAIN__SHARD__ARENA_CAPACITY_BYTES: "4GiB"
    deploy:
      resources:
        limits:
          cpus: "8"
          memory: 32G
```

See [`../tuning/shard-sizing.md`](../tuning/shard-sizing.md) for
when this is the right move.

---

## Production caveats

This compose stack is good for **single-host production** and
**development**. It is **not** good for:

- HA — there is no clustering; one host loss = downtime.
- Cross-AZ — all four services live on one network namespace.
- Long-term metrics retention — Prometheus uses local TSDB with
  15-day retention. Ship to Mimir / Cortex / Thanos for longer.
- Multi-tenant — Grafana ships with an anonymous viewer; that's
  fine for a private network but not for a public host.

For multi-host topology, treat this compose file as a worked
example and translate to your orchestrator
([`kubernetes.md`](kubernetes.md) sketches the K8s shape).

---

## Troubleshooting

### `prometheus` target shows `DOWN` for `brain`

```bash
docker compose logs brain | tail
```

If Brain is logging healthy lines, network is the issue. From the
Prometheus container:

```bash
docker exec brain-prometheus wget -qO- http://brain:9091/metrics | head
```

If that fails, the compose network is broken — `docker compose
down && docker compose up -d` typically fixes it.

### `grafana` shows no dashboards

Confirm provisioning loaded:

```bash
docker compose logs grafana | grep -i dashboard
```

You should see one `provisioning dashboard from file` line per
JSON in `monitoring/dashboards/`. If you see permission errors,
check that the mount paths exist on the host.

### Brain restarts on a loop

Probably a config error — its healthcheck never goes healthy,
restart policy kicks in.

```bash
docker compose logs brain | head -50
```

Look for `config error:` near the top. Fix the override, then
`docker compose up -d brain`.

---

## See also

- [`docker.md`](docker.md) — production single-container shape
  (this page builds on it).
- [`../observability.md`](../observability.md) — full metrics +
  tracing + logging guide (not compose-specific).
- [`../../runbooks/`](../../runbooks/) — incident response.
- [`../../../monitoring/README.md`](../../../monitoring/README.md)
  — observability assets that work in *any* deployment shape,
  not just this compose stack.
