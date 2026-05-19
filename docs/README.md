# Brain documentation

Welcome. Pick the path that matches what you're trying to do.

## I'm new to Brain — show me what it does

Start with the [5-minute Docker quickstart](tutorials/01-quickstart-docker.md).
You'll have Brain running locally before your coffee cools.

Then read [`concepts/overview.md`](concepts/overview.md) — what
Brain *is*, the two-layer model, why the API verbs are
`encode/recall/plan/reason/forget` instead of CRUD.

If you prefer code-first, jump to the
[first substrate app tutorial](tutorials/02-first-substrate-app.md).

## I want to put Brain into production

The deployment guides:

| Shape | Guide |
|---|---|
| Single Docker container | [`guides/deployment/docker.md`](guides/deployment/docker.md) |
| Full stack with Prometheus + Grafana | [`guides/deployment/docker-compose.md`](guides/deployment/docker-compose.md) |
| systemd on a VM | [`guides/deployment/systemd.md`](guides/deployment/systemd.md) |
| Kubernetes StatefulSet | [`guides/deployment/kubernetes.md`](guides/deployment/kubernetes.md) |

Don't skip:

- [`guides/configure.md`](guides/configure.md) — the common config fields.
- [`guides/security/`](guides/security/) — Brain ships `auth = none`; read this *before* exposing port 8080.
- [`guides/observability.md`](guides/observability.md) — wiring metrics, traces, logs.
- [`guides/deployment/backup-restore.md`](guides/deployment/backup-restore.md) — DR drill before your first production write, not after the first incident.

## Something's wrong — I need a runbook

[`runbooks/README.md`](runbooks/README.md) — RB-1 through RB-11.
Alert annotations point here.

## I need to tune Brain for my workload

[`guides/tuning/`](guides/tuning/) — HNSW parameters, shard
sizing, WAL knobs, embedder throughput. Tune *after* measuring,
not by intuition.

## I want to use the `brain` interactive shell

- **First time?** → [`tutorials/03-shell-deep-dive.md`](tutorials/03-shell-deep-dive.md) — 20-minute guided tour.
- **Workflow recipes** → [`guides/shell/`](guides/shell/) — named agents, subscribe replay, bulk encode, JSON scripting, troubleshooting.
- **Look up a flag / output field** → [`reference/brain-shell.md`](reference/brain-shell.md) (overview) + [`reference/shell/`](reference/shell/) (commands, REPL meta, output formats, configuration, errors).

## I need exact information — config fields, opcodes, error codes

[`reference/`](reference/) — the look-it-up tier:

- [`reference/configuration.md`](reference/configuration.md) — every TOML field
- [`reference/brain-shell.md`](reference/brain-shell.md) — `brain` shell overview (deep ref in [`reference/shell/`](reference/shell/))
- [`reference/cli.md`](reference/cli.md) — `brain-cli` admin subcommands
- [`reference/http-api.md`](reference/http-api.md) — `/healthz`, `/metrics`, `/v1/*`
- [`reference/wire-protocol/`](reference/wire-protocol/) — frame format, opcodes, error codes
- [`reference/cognitive-operations/`](reference/cognitive-operations/) — exact semantics of ENCODE/RECALL/PLAN/REASON/FORGET
- [`reference/schema-dsl/`](reference/schema-dsl/) — schema grammar
- [`reference/sdk-rust.md`](reference/sdk-rust.md) — public SDK surface
- [`reference/metrics.md`](reference/metrics.md) — Prometheus metric catalog
- [`reference/performance.md`](reference/performance.md) — latency + throughput targets

## I want to understand *why* Brain works the way it does

[`concepts/`](concepts/) — the explanation tier. What the
vocabulary means, why the design is what it is.

For deep internals (storage layout, WAL group commit, HNSW
rationale), read [`architecture/`](architecture/) — twelve
numbered chapters that distil the spec into reader-facing prose.

## I'm contributing — build, test, debug

[`development/`](development/):

- [`development/usage/`](development/usage/) — build, run, debug, test workflow.
- [`development/spec-deviations.md`](development/spec-deviations.md) — where the implementation knowingly diverges from the spec.
- [`development/phases/`](development/phases/) — per-phase plans and sub-task histories.

## I want acceptance evidence — does Brain hit its targets?

[`benchmarks/`](benchmarks/) — per-phase result reports,
methodology, durability criteria.

## Where else to look

- [`../spec/`](../spec/) — the authoritative design. The spec is
  read-only; code disagreements get fixed in the code, not the
  spec.
- [`../ROADMAP.md`](../ROADMAP.md) — phase index.
- [`../CHANGELOG.md`](../CHANGELOG.md) — release history.
- [`../monitoring/`](../monitoring/) — Grafana dashboards +
  Alertmanager rules (deployment assets, not docs).
- [`../CONTRIBUTING.md`](../CONTRIBUTING.md) — how to contribute.

## Layout

Brain's docs follow the [Diátaxis framework](https://diataxis.fr/),
extended with two Brain-specific tiers:

| Tier | Audience | Goal |
|---|---|---|
| [`tutorials/`](tutorials/) | New users | **Learning** by doing |
| [`guides/`](guides/) | Operators / integrators | **Getting things done** |
| [`reference/`](reference/) | Everyone | **Looking things up** |
| [`concepts/`](concepts/) | Evaluators / curious users | **Understanding** what & why |
| [`architecture/`](architecture/) | Engineers | **Understanding internals** (deep dive) |
| [`runbooks/`](runbooks/) | Operators in an incident | **Resolving a problem** |
| [`benchmarks/`](benchmarks/) | Release managers / evaluators | **Acceptance evidence** |
| [`development/`](development/) | Contributors | **Working on Brain itself** |

If a document doesn't fit one of those buckets cleanly, it
probably belongs in [`../spec/`](../spec/) (authoritative design)
or in inline rustdoc (API reference).
