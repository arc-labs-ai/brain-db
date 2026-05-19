# Guides

**Audience:** operators and integrators who already understand
what Brain does and want to *get something done*.

**Goal:** *task accomplishment*. Each page is goal-oriented:
"install Brain", "harden the network surface", "tune HNSW for
recall". Not background reading (see [`../concepts/`](../concepts/)),
not exhaustive reference (see [`../reference/`](../reference/)).

## Top-level guides

| Guide | When you'd read it |
|---|---|
| [`install.md`](install.md) | First time installing Brain on a host |
| [`configure.md`](configure.md) | Tuning `[server]`, `[storage]`, `[shard]`, `[hnsw]`, `[embedder]`, `[workers]` — the common fields. Full schema lives in [`../reference/configuration.md`](../reference/configuration.md). |
| [`operate.md`](operate.md) | Day-to-day: starting, stopping, restarting, log handling |
| [`upgrade.md`](upgrade.md) | Moving between Brain versions safely |
| [`observability.md`](observability.md) | Wiring metrics, traces, logs into your stack |

## Subtrees

- [`deployment/`](deployment/) — Putting Brain on infrastructure.
  Docker, docker-compose, systemd, Kubernetes, TLS, backup/restore.
  Start with [`deployment/docker.md`](deployment/docker.md) for a
  fresh install; [`tutorials/01-quickstart-docker.md`](../tutorials/01-quickstart-docker.md)
  is the friendlier on-ramp.
- [`security/`](security/) — Hardening: network surface, auth
  modes, data-at-rest. Read before exposing Brain past loopback.
- [`tuning/`](tuning/) — Workload-specific tuning. HNSW
  parameters, shard sizing, WAL knobs, embedder throughput.
  Tune *after* measuring; do not tune by intuition.
- [`sdk/`](sdk/) — Using the Brain Rust SDK from your application
  code. Quickstart, connection pooling, typed knowledge.
- [`shell/`](shell/) — Workflow playbooks for the `brain`
  interactive shell. Named agents, subscribe + replay, bulk
  encode, JSON scripting with jq, troubleshooting.

## What goes where

| You want to … | Look in |
|---|---|
| Get Brain running on a fresh host | [`install.md`](install.md) + [`deployment/`](deployment/) |
| Change Brain's behaviour | [`configure.md`](configure.md) + [`tuning/`](tuning/) |
| Connect Brain to your app | [`sdk/`](sdk/) |
| Use the `brain` interactive shell | [`shell/`](shell/) + [`../reference/brain-shell.md`](../reference/brain-shell.md) |
| Make Brain safe in production | [`security/`](security/) |
| Recover from an incident | [`../runbooks/`](../runbooks/) |
| Look up exact field semantics | [`../reference/`](../reference/) |
