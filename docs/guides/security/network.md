# Network exposure

How to bind Brain's listeners so the right things reach the right
people. v1 has **no built-in authentication** on either HTTP
surface — every protection here is network-perimeter discipline.

## The three sockets

| Field | Default | What it serves | Bind on |
|---|---|---|---|
| `listen_addr` | `127.0.0.1:8080` (dev) / `0.0.0.0:8080` (container) | Data plane: rkyv wire protocol. Clients + SDK speak this. | Private interface or `0.0.0.0` behind a proxy. **Never raw on the public internet** until auth lands. |
| `metrics_addr` | `127.0.0.1:9091` (dev) / `0.0.0.0:9091` (container) | Public HTTP: `GET /healthz`, `GET /metrics` (Prometheus text). | Reachable by your load balancer + scraper. Read-only, benign. |
| `admin_addr` | `127.0.0.1:9092` everywhere | Admin HTTP: `GET/POST /v1/snapshots`, `POST /v1/rebuild-ann`, `GET /v1/workers`, `GET/POST /v1/config`, `GET /v1/audit`, `GET /v1/agents`, `GET /v1/shards`, `GET/POST /v1/diagnostics/*`. | **Loopback only.** v1 has no admin auth — anyone reachable can dump your audit log, trigger snapshots, change config. |

## Why the split exists

`/healthz` and `/metrics` exist to be scraped — a Prometheus
server on another host needs to reach them. `/v1/audit` exists to
let you dump the audit log for investigation — a Prometheus
server on another host should **not** reach that.

Splitting onto two listeners lets a network firewall enforce the
distinction with a one-line rule: `allow 9091, deny 9092`. With
one combined listener you would need a reverse proxy or WAF in
front, doing per-path filtering, and a bug in that filter would
silently expose the admin surface.

The `admin_addr = 127.0.0.1:...` default is deliberate. If
you change it to a non-loopback interface, you have explicitly
chosen to do so — make sure something else is gating it.

## Reaching admin from outside the host

Three safe patterns, ordered by complexity:

1. **`ssh + brain-cli`** — `ssh brain-host brain-cli worker list`.
   The CLI runs on the box, talks to `127.0.0.1:9092` from there.
2. **`docker exec`** — inside a container deployment, admin lives
   on container-loopback and is reached via
   `docker exec brain brain-cli worker list`. The host can't hit
   `9092`; that's the point.
3. **Reverse proxy with mTLS** — bind `admin_addr` to a private
   interface (`10.0.0.5:9092`) and front it with an nginx / Envoy
   that requires a client certificate. Lets your operator network
   reach admin without ever exposing it on the internet.

## Anti-patterns

- **Binding `admin_addr` to `0.0.0.0`** without a proxy in front
  of it. v1 has no admin auth; you have just published your audit
  log + config-mutation endpoints to whoever can reach the host.
- **EXPOSE'ing the admin port in your Dockerfile.** The shipped
  `Dockerfile` deliberately doesn't (`EXPOSE 8080 9091`). If you
  add `EXPOSE 9092` you're documenting an intent to publish that
  port, and orchestrators (compose, k8s) may act on it.
- **Letting `metrics_addr` and `admin_addr` collide on the same
  port.** Earlier code on this repo had `admin_addr` declared but
  not wired — operators who set both to the same port silently
  got the unified server's behaviour. Both fields now bind real
  listeners; configuring both to the same port returns
  `AddrInUse` on startup. Fail loud is the intended behaviour.

## When admin auth lands

Spec §14/06 and spec §03/06 describe an admin-token model
distinct from agent tokens, gated at handshake. Once that lands,
binding `admin_addr` to a public interface becomes safe **if** you
configure tokens. Until then, the network boundary is the only
boundary; this page describes how to maintain it.

## See also

- [`auth-modes.md`](auth-modes.md) — `none` / `token` / `mtls` —
  what's wired today, what's coming.
- [`../deployment/docker.md`](../deployment/docker.md) — what
  ports the container exposes (and which ones it doesn't).
- [`../../reference/configuration.md`](../../reference/configuration.md)
  — every config field with full notes.
- [`../../../spec/14_observability_ops/06_admin_ops.md`](../../../spec/14_observability_ops/06_admin_ops.md)
  — authoritative design for admin operations.
