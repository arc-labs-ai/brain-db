# Brain v1.0.0 — Release notes

**Status:** scaffolded; awaiting the operator-run 48 h soak result
+ the `v1.0.0` tag. This document is the operator-facing summary;
the full developer-facing changelog lives in
[`CHANGELOG.md`](CHANGELOG.md).

## What Brain is

Brain is a **cognitive substrate for AI agents** — a database
where the primitives are cognitive operations (ENCODE, RECALL,
PLAN, REASON, FORGET) instead of tables / documents / vectors.

v1.0.0 is the first stable release. The wire protocol, on-disk
format, and SDK surface are SemVer-stable from this tag forward.

## Highlights

- **End-to-end cognitive substrate**: encode, recall, plan,
  reason, forget, link, transactions, subscriptions.
- **Production durability**: WAL-before-acknowledge with O_DIRECT
  + group commit; CRC-validated recovery; 1000-iteration chaos
  recovery suite green.
- **Per-core scaling**: thread-per-core Glommio executor per
  shard; io_uring for storage; single-writer-per-shard discipline.
- **Full observability**: Prometheus metrics taxonomy, JSON-
  structured logs, OpenTelemetry tracing, 8 Grafana dashboards,
  Alertmanager rules.
- **Operator-ready**: 10 runbooks, install / configure / operate /
  upgrade guides, admin CLI, acceptance gate runner.

## What's new (compared to no prior version)

This is the first numbered release. The phase-by-phase build
history is in `CHANGELOG.md`.

## Performance characteristics

Spec §16/02 targets at 1 M memories per shard:

| Operation | p99 latency target |
|---|---|
| ENCODE | 25 ms |
| RECALL (K=10, no text) | 20 ms |
| RECALL (K=10, with text) | 30 ms |
| PLAN (depth 3) | 18 ms |
| REASON (depth 3) | 35 ms |
| FORGET | 15 ms |
| LINK / UNLINK | 10 ms |

Acceptance gate 5 verifies these on reference hardware (16-core
x86_64, 64 GB RAM, NVMe). Operators running on different hardware
should expect to characterise their own baselines using
`crates/brain-sdk-rust/examples/load_generator.rs`.

## Compatibility

- **Linux only.** Kernel ≥ 5.15. The Linux-only stance is
  deliberate — spec §01/05 §1.1 documents why.
- **Single-host per shard.** v1 doesn't ship clustering; v2 will
  add quorum-based multi-host. RB-10 scaffolds the v2 procedure
  shape.
- **MSRV:** Rust stable, latest minus one (currently 1.95).

## Upgrading from pre-1.0

Pre-1.0 development tags (`phase-NN-complete`) didn't ship a
stable wire / data format. **No automatic migration path from
pre-1.0 data.** For fresh deployments only.

Within the v1.x line, follow [`docs/guides/upgrade.md`](docs/guides/upgrade.md).

## Security posture

- TLS termination on the data port via rustls — optional,
  configured via `[server] tls_cert_file` / `_key_file`.
- Authentication: **none in v1**. Token / mTLS are the immediate
  follow-up. Until then, brain-server's network policy is:
  **don't expose it to the public internet**. Bind to private
  interfaces; put a reverse proxy with auth in front.
- Wire-protocol fuzzing: `cargo-fuzz` targets for Frame,
  RequestBody, ResponseBody, handshake; ~67 M iterations clean
  at release time.
- Default `unsafe` posture: only `crates/brain-storage` uses
  `unsafe` (mmap / pointer arithmetic on the arena); every block
  carries a `// SAFETY:` comment.

## Known limitations

The full list is in `CHANGELOG.md` under "Known limitations".
The headline items:

- **No client→server trace propagation.** Spec §14/03 §8 requires
  a wire-protocol amendment; v1 emits server-side spans only.
  Tracker: `phase-13/wire-traceparent`.
- **Audit log not yet wired.** The admin CLI surface returns 501
  for audit query / export. Tracker: `phase-11/audit-log`.
- **Several spec'd metrics deferred** behind the primitives that
  back them (HNSW sampling, storage stats, embedder hooks,
  executor latency). Each carries a `phase-12/<slug>` tracker
  inline in the source.

These are scoped, documented, and tracked — none are silent gaps.

## What's next

- v1.x will land the auth surfaces (token / mTLS), the deferred
  observability primitives, and the wire-protocol traceparent
  field.
- v2 adds clustering.

## Getting started

- Install: [`docs/guides/install.md`](docs/guides/install.md)
- Configure: [`docs/guides/configure.md`](docs/guides/configure.md)
- Operate: [`docs/guides/operate.md`](docs/guides/operate.md)
- Monitor: [`docs/observability.md`](docs/observability.md)

## Acknowledgements

Brain stands on:
- [Glommio](https://github.com/DataDog/glommio) — thread-per-core
  io_uring-driven async.
- [hnsw_rs](https://github.com/jean-pierreBoth/hnswlib-rs) — the
  ANN index core.
- [candle](https://github.com/huggingface/candle) — the embedder
  runtime.
- [redb](https://github.com/cberner/redb) — the metadata store.
- [hyper](https://github.com/hyperium/hyper) — the HTTP substrate
  the admin layer rides on.

The 17-document specification (~42K lines, ~218 markdown files)
under `spec/` is the design contract this release implements.

---

*See `CHANGELOG.md` for the full developer changelog.
See `docs/guides/` for operator setup.
See `docs/runbooks/` for failure-mode procedures.*
