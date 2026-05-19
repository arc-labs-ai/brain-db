# Deployment

**Audience:** operators putting Brain on real infrastructure.

**Goal:** pick the deployment shape that fits your environment and
get it running cleanly the first time.

Brain is **Linux-only** (kernel ≥ 5.15) — spec §01/05 §1.1
explains why (io_uring, O_DIRECT, pwritev2 RWF_DSYNC). On macOS or
Windows hosts, run Brain in a container.

## Choose your shape

| Shape | Use when | Guide |
|---|---|---|
| Docker (single container) | Dev box, smoke tests, simple prod | [`docker.md`](docker.md) |
| docker-compose (stack) | Single-host prod with Prometheus + OTel | [`docker-compose.md`](docker-compose.md) |
| systemd on a VM | Bare-metal / cloud VM with established systemd ops | [`systemd.md`](systemd.md) |
| Kubernetes (StatefulSet) | Container platform with existing K8s ops | [`kubernetes.md`](kubernetes.md) |

For a "5-minute first run" walkthrough rather than a production
checklist, jump to
[`../../tutorials/01-quickstart-docker.md`](../../tutorials/01-quickstart-docker.md).

## Cross-cutting

- [`tls.md`](tls.md) — Terminating TLS on the data port, cert
  rotation, when to put a reverse proxy in front instead.
- [`backup-restore.md`](backup-restore.md) — Snapshot, restore, DR
  drill. Read **before** the first production write, not after the
  first incident.

## What this section assumes you already know

- That Brain stores data in a `data_dir` (see [`../configure.md`](../configure.md)).
- The three ports: `listen_addr` (data plane, default 8080),
  `metrics_addr` (public HTTP — /healthz + /metrics, default 9091),
  `admin_addr` (admin HTTP — /v1/* routes, default 9092 — keep on
  loopback in production; v1 has no built-in admin auth).
- That auth is currently `none` (token / mTLS are deferred — see
  [`../security/auth-modes.md`](../security/auth-modes.md)). Do
  not expose port 8080 to the public internet without a reverse
  proxy that authenticates.

## See also

- [`../../reference/configuration.md`](../../reference/configuration.md)
  — every config field, every default.
- [`../../guides/observability.md`](../observability.md) — wiring
  the OTel collector / Prometheus / Grafana stack into whichever
  deployment shape you picked.
- [`../../runbooks/substrate-down.md`](../../runbooks/substrate-down.md)
  — first-stop runbook when Brain won't come up.
