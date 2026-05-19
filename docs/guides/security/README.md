# Security

**Audience:** anyone exposing Brain beyond a loopback interface.

**Goal:** *hardening*. Reduce the blast radius if something goes
wrong. Not vulnerability reporting (see
[`../../../SECURITY.md`](../../../SECURITY.md) for that).

## State of the world

Brain v1.0 ships with **`mode = "none"`** as the only fully-wired
auth mode. Token and mTLS auth are designed (spec §03/06
handshake) but deferred to Phase 14+. Until they land, Brain's
network policy
must be enforced at the boundary — not inside Brain.

That means: **never expose `listen_addr` directly to the public
internet**. Either:

1. Bind Brain to a private interface and route through a reverse
   proxy that authenticates and rate-limits, or
2. Sit Brain behind a service mesh (Envoy / Linkerd) that
   enforces mTLS at the mesh boundary.

## Pages

| Page | Read when |
|---|---|
| [`network.md`](network.md) | Choosing bind addresses, reverse-proxy patterns, NetworkPolicy on K8s |
| [`auth-modes.md`](auth-modes.md) | What `none` / `token` / `mtls` actually do, when each lands |
| [`data-at-rest.md`](data-at-rest.md) | Disk encryption, file perms on `data_dir`, WAL handling |

## See also

- [`../deployment/tls.md`](../deployment/tls.md) — terminating
  TLS on the data port if you don't have a proxy.
- [`../../../spec/03_wire_protocol/06_handshake.md`](../../../spec/03_wire_protocol/06_handshake.md)
  — authoritative auth design (handshake-level capability negotiation).
- [`../../../SECURITY.md`](../../../SECURITY.md) — how to report
  a vulnerability to the maintainers.
