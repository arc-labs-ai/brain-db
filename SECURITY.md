# Security policy

## Supported versions

| Version | Supported |
|---|---|
| 1.0.x | ✓ |
| < 1.0 | ✗ (pre-release; upgrade) |

Security fixes target the latest `1.x` minor release. We
backport to the previous minor release for 90 days after a
new minor lands.

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

- The TCP / WebSocket / SSE transport layer.
- The CBOR-decoded request bodies.
- The schema DSL parser (when a schema is uploaded).
- The LLM extractor (when configured) — prompts are derived
  from memory text, which is itself untrusted.

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
  [`spec/11_extractors/`](spec/11_extractors/00_purpose.md) + cost budget
  caps).
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
  `(namespace, agent_id, permissions)` strictly from the looked-up key
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
- **Bound per-connection and per-shard resources.** Tune the
  connection limits (`max_connections`, `max_connections_per_ip`,
  `max_payload_bytes`, and the read/auth/idle/ping timeouts) and the
  per-shard storage caps (`[shard]` `arena_capacity_bytes`,
  `wal_segment_size_bytes`, `wal_retention_segments`) to your host so a
  single noisy or hostile peer cannot exhaust the box.
- **Disable extractor / rerank tiers you don't use.** An *enabled*
  capability tier (`[rerank]`, `[extractors.*]`, `[llm]`) that fails to
  load is a hard shard-spawn failure; leave tiers you don't need
  disabled so you don't depend on an external LLM endpoint or model
  file you can't guarantee at startup.

## Public CVE history

None yet. v1.0.0 is the first release.
