# Security policy

## Supported versions

Brain is pre-release. Security fixes land on the latest `0.x`
release and nowhere else — there is no backport window, because
pre-1.0 changes are made in place with no compatibility shims.
Run the newest tag.

| Version | Supported |
|---|---|
| Latest `0.x` release | ✓ |
| Any earlier release | ✗ — upgrade |

Once `1.0` ships, fixes will target the latest `1.x` minor with a
backport window to the previous minor.

## Reporting a vulnerability

**Do not** open a public GitHub issue for security reports.

Email security disclosures to the maintainer at the address
listed on the GitHub profile, or open a private security
advisory via GitHub's
[Security Advisories](https://github.com/arc-labs-ai/brain-db/security/advisories)
tab. Include:

- A description of the issue + impact.
- A reproducer (proof of concept, exploit script, or minimal
  test case).
- Affected version(s).
- Any suggested mitigation.

We aim to acknowledge within **3 business days** and ship a
fix within **30 days** for confirmed issues. Coordinated
disclosure is appreciated; we'll work with you on an embargo
window.

## Threat model

Brain runs **untrusted wire input** through:

- The data-plane TCP listener (optionally TLS 1.3): the 32-byte
  frame header, its CRC, and the length field.
- The CBOR-decoded request bodies.
- The schema DSL parser (when a schema is uploaded).
- The LLM extractor — prompts are derived from memory text, which
  is itself untrusted. Extraction is always on; there is no
  configuration in which this path is absent.

The HTTP listeners are a separate surface: the public one serves
`/healthz`, `/readyz` and `/metrics` with no request body to parse,
and `/v1/*` is operator-only and Bearer-gated.

The substrate assumes:
- **Trusted disk** (no malicious filesystem behind the data
  directory).
- **Trusted operator** (the admin endpoint has authority over
  the deployment).
- **TLS in transit** when deployed outside a trust boundary.

Threats we model:

- Malformed wire frames (CRC + CBOR decode reject).
- WAL corruption (CRC32C detects; recovery refuses corrupt
  records).
- Idempotency-key collisions (`RequestId` UUID v7; spec §02/03).
- LLM prompt injection (extractor sandboxing per
  [`spec/11_extractors/`](spec/11_extractors/00_purpose.md)). Note that
  spend is **recorded, never capped** — see "What is not a
  control" below.
- Out-of-bounds resource use (per-shard quotas, group commit
  caps, bounded queues with overflow metrics).

Threats out of scope:

- A malicious operator with shell access to the data
  directory.
- Side-channel attacks against the host (CPU, RAM, disk).
- Adversarial training data poisoning of the embedding model
  (we ship a pinned model fingerprint; rotation is
  operator-driven).

## Production deployment hardening

Brain **fails closed on authentication** — there is no permissive or
anonymous mode, and none can be configured. What's still on the
operator is mostly about network exposure and resource limits. Before
exposing a server beyond `localhost` / a trusted LAN, walk this
checklist:

- **Data-plane auth can't be turned off, but keys still need
  managing.** Every connection must present a valid, resolvable,
  non-revoked API key in the wire AUTH frame; the server resolves
  `(namespace, space, permissions)` strictly from the looked-up key
  row, never from anything the client claims. There's no `[auth] mode`
  config and no env var that weakens this. Mint and revoke keys via
  the admin HTTP surface, and treat every minted key as a capability
  grant — scope and rotate them deliberately.
- **The admin bootstrap secret is required at startup.** Set
  `[admin] token` (or `BRAIN__ADMIN__TOKEN`) before starting the
  server — the admin listener mints and revokes data-plane API keys,
  so it must never run unauthenticated. If the secret is unset, the
  server refuses to start (a hard error and nonzero exit) rather than
  come up open. Every `/v1/*` admin call must present that secret as
  `Authorization: Bearer <token>`; a missing or wrong token is
  rejected. `/healthz`, `/readyz`, and `/metrics` are intentionally
  unauthenticated by design and live on the separate public listener
  (`server.metrics_addr`, default `127.0.0.1:9091`) — they carry no
  sensitive data.
- **Enable TLS across a trust boundary.** Set `[server.tls] enabled =
  true` with `cert` and `key` paths whenever traffic leaves the host
  or trusted LAN. Without TLS, wire frames (including API-key tokens)
  travel in cleartext.
- **Keep the admin listener loopback-only regardless.** The admin HTTP
  endpoint (`server.admin_addr`, default `127.0.0.1:9092`) is
  Bearer-token-gated, but a leaked admin token is a full deployment
  compromise — don't compound that risk by also binding it to a
  routable interface. Front it with a reverse proxy that adds a second
  authentication layer if remote admin access is genuinely required.
- **Respect the trusted-disk / trusted-operator assumptions.** The
  data directory must sit on a trusted filesystem and anyone with shell
  access to it has full authority over the deployment (see the threat
  model above). Restrict OS-level access to the data dir and the
  process accordingly.
- **Size the per-shard storage caps to your host.** `[shard]`
  `arena_capacity_bytes`, `wal_segment_size_bytes` and
  `wal_retention_segments` are the per-shard ceilings on resident
  vectors and on-disk WAL, and they are multiplied by
  `[storage] shard_count`. Set them so the whole shard tier fits the
  box with headroom.
- **Know which limits you cannot set.** The connection-layer bounds —
  process-wide and per-IP connection caps (4096 / 64), the 16 MiB − 1
  payload ceiling, per-connection stream cap (1024), and the
  read / auth / idle / ping timeouts (30 s / 30 s / 300 s / 30 s) —
  are compiled in, not configuration
  (`crates/brain-server/src/network/connection.rs:157-172`, constructed
  from `::default()` at `crates/brain-server/src/lib.rs:495`). They are
  enforced: excess connections are shed at accept time before any TLS or
  handshake work, and a connection over the stream cap gets
  `StreamLimitExceeded`. But they cannot be tightened for a hostile
  network, so put a network-level limiter in front of a data plane that
  faces one. **Do not try to add them to the config file** — every config
  struct is `#[serde(deny_unknown_fields)]`, so an invented key is a
  refusal to boot.
- **Leave `[rerank]` disabled.** It is the one capability tier with an
  enable flag, and turning it on without a cross-encoder model is a
  fatal `CrossEncoderInitFailed` at boot rather than a degradation — the
  published image carries no such model. There is no corresponding knob
  for extraction: the three extractor tiers are always on, because the
  typed graph that reads fuse is what they populate, and the classifier
  tier's model is required for the server to start at all.

## What is not a control

Three things an operator might reasonably expect to be security
controls, and are not:

- **LLM spend is recorded, never capped.** A namespace's model-backed
  stages bill the credential that namespace registered, so a tenant's
  own key is never rationed on their behalf. Spend accumulates in a
  durable `llm_spend` ledger per namespace and stage
  (`crates/brain-metadata/src/llm_spend/ops.rs`), which is an audit
  trail, not a gate: no call is refused for cost. A `cost_budget` may
  be declared on an `llm` extractor in the schema DSL and parses, but
  it is explicitly unenforced — materialization logs it as such
  (`crates/brain-extractors/src/materialize.rs:438-455`). Rate and
  spend limiting belong to whatever holds the provider key. (The
  planner's `cost_budget_ms` is unrelated and is enforced; it is a
  query-latency budget, not money.)
- **The connection-layer caps are not tunable.** See the hardening
  checklist above.
- **The sealing key does not protect the data directory.** Unset, it is
  generated into that directory on first boot, which protects a copied
  volume or a backup archive — neither carries the key file — and
  nothing more. `BRAIN__LLM__CREDENTIALS__KEK` is what puts it
  somewhere Brain's disk is not.

## Public CVE history

None yet.
