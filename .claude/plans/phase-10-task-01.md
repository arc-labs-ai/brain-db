# Sub-task 10.1 — `Client` skeleton

**Reads:**
- `spec/13_sdk_design/00_purpose.md` (SDK contract).
- `spec/13_sdk_design/01_principles.md` (design constraints).
- `spec/13_sdk_design/02_core_api.md` §1, §2, §14 (Client shape, connection management, defaults).
- `spec/13_sdk_design/03_connection.md` §1-§5 (single-connection lifecycle).
- `spec/03_wire_protocol/06_handshake.md` (HELLO/WELCOME/AUTH FSM the client must complete).

**Phase doc:** `docs/phases/phase-10-sdk-cli.md` §10.1.

**Done when:** `Client::connect(addr)` opens a TCP connection to a
running brain-server, completes HELLO → WELCOME → AUTH → AUTH_OK,
and returns a `Client` value the caller can use for follow-up
operations (BYE for now; 10.5 wires ENCODE/RECALL/etc.). An
integration test spins up an in-process server, connects, sends
BYE, and asserts a clean shutdown.

---

## 1. What 10.1 actually delivers

The crate is currently a placeholder (`SPEC_REFERENCE` const,
single unit test). 10.1 lands the first real surface:

- `Client` type that owns a single TCP connection.
- `Client::connect(addr) -> Result<Client, ClientError>` builder
  + async constructor.
- `Client::bye(self) -> Result<(), ClientError>` to close cleanly
  (BYE frame).
- `ClientError` enum covering the failure modes 10.1 can hit:
  `Connect`, `Handshake`, `Auth`, `Io`, `Protocol`, `Closed`.
- `ClientConfig` builder with the spec §14 defaults:
  - `timeout = 30 s`
  - `retries = 3` (placeholder; 10.3 makes it work)
  - `backoff_initial = 100 ms` (placeholder; 10.3)
  - `auth = AuthMethod::None` for v1 dev (per spec §03/06 §3.1).
- The HELLO/WELCOME/AUTH/AUTH_OK handshake driven by reusing
  `brain-protocol`'s `Frame` + `RequestBody` + `ResponseBody` types
  (no re-implementation).

What 10.1 explicitly does NOT deliver (later sub-tasks):

- Connection pool (10.2).
- Retry with backoff + jitter (10.3 — config fields are
  placeholders).
- Auto-generated UUIDv7 RequestIds (10.4).
- ENCODE / RECALL / PLAN / REASON / FORGET / LINK / UNLINK / TXN
  / SUBSCRIBE methods (10.5 / 10.6).
- Streaming + back-pressure (10.6).
- tracing spans + OTLP attributes (10.7).
- TLS (deferred: dev path uses plain TCP; spec §13/03 §3 mentions
  TLS as a config knob but the integration test runs on
  127.0.0.1:0 with `auth = None`).
- Reconnect logic, idle keep-alive (deferred to 10.2).

The intent is to make every following sub-task incremental: add
features by extending `Client`'s impl, not by re-architecting
the type.

---

## 2. Implementation choice — Tokio + brain-protocol re-use

**Runtime**: Tokio. The SDK is async-first per spec §13/00 §6,
and brain-server's connection layer uses Tokio too. There's no
reason to introduce a second runtime.

**Wire layer**: re-use `brain-protocol::Frame`, `Header`,
`RequestBody`, `ResponseBody`. The connection writes
`Frame::encode().write(stream)` and reads via
`Frame::decode_with_max`. Same code path as the server's per-
connection task. No duplicate framing logic.

**Handshake**: hand-rolled FSM (3 frame round-trips). The
matching code lives at `crates/brain-server/src/network/dispatch.rs:dispatch_frame`
(server side) — refactoring that into a shared crate is a v2
concern; for now the SDK has its own minimal client-side FSM
(50–80 LOC).

**Frame I/O abstraction**: keep it concrete (`tokio::net::TcpStream`)
not generic over `AsyncRead + AsyncWrite`. v1 doesn't need TLS;
10.2's pool will likely require some abstraction but the
introduction is cheaper when 10.2 lands than baked in 10.1.

---

## 3. Module layout

Per the saved feedback memory (`feedback_src_folder_layout.md`),
every concern lives in its own folder under `src/`. Only `lib.rs`
sits at the root. We design this from the start so the SDK
doesn't need a reorg later when 10.2–10.7 add modules.

```
crates/brain-sdk-rust/
├── src/
│   ├── lib.rs              crate entry; module declarations + re-exports
│   ├── client/
│   │   └── mod.rs          Client type + connect/bye (10.1)
│   │                       (later: per-op methods land here in 10.5)
│   ├── config/
│   │   └── mod.rs          ClientConfig + AuthMethod re-export
│   ├── error/
│   │   └── mod.rs          ClientError + From impls
│   └── proto/
│       ├── mod.rs          re-exports the brain-protocol bridge
│       └── handshake.rs    client-side HELLO → AUTH_OK FSM
└── tests/
    └── connect.rs          in-process server scaffold + happy path
```

Folders for future sub-tasks (added empty/with placeholder `mod.rs`
when they land — not in 10.1):

- `pool/` (10.2 — connection pool, ServerConnections)
- `retry/` (10.3 — backoff + jitter)
- `request_id/` (10.4 — UUIDv7 generator + per-call override)
- `ops/` (10.5 — encode/recall/plan/reason/forget/link/unlink/txn)
- `stream/` (10.6 — subscribe async iterator)
- `tracing/` (10.7 — spans + OTLP attributes)

`Cargo.toml` adds the deps in §4. `lib.rs` declares the
sub-modules + re-exports the public types.

LOC estimates: client/mod.rs ~200, config/mod.rs ~80,
error/mod.rs ~60, proto/handshake.rs ~120, lib.rs ~30,
tests/connect.rs ~250.

---

## 4. Cargo deps to add

- `tokio` — workspace dep; features `["net", "io-util", "macros", "rt-multi-thread"]`.
- `brain-protocol` — path dep.
- `brain-core` — path dep (already there).
- `thiserror` — already there.
- `tracing` — workspace dep.
- `uuid` — workspace dep (kept now so 10.4 doesn't need a Cargo.toml change).

`brain-storage`, `brain-metadata`, etc. — **not** SDK deps. The
SDK is pure-client.

---

## 5. Tests

### 5.1 `tests/connect.rs::connects_to_running_server`

Spins up brain-server in the same process (re-using the
existing `tests/e2e.rs` scaffold pattern — `start_with_shards`
on `127.0.0.1:0`), then:

1. `let client = Client::connect(addr).await?;`
2. Assert `client` reports `connected = true`.
3. `client.bye().await?;`
4. Server's listener completes within 2 s.

This proves the handshake completes against a real server and
the BYE-driven close is clean.

### 5.2 `tests/connect.rs::handshake_failure_propagates`

Connect to a `127.0.0.1:0` socket that's been bound + dropped
(connection refused), assert `ClientError::Connect(_)`.

### 5.3 Unit tests in `client.rs` and `handshake.rs`

- `ClientConfig::default()` returns the spec defaults.
- `ClientError::is_retryable()` returns expected values per the
  spec §13/04 list (placeholder for 10.3, but the mapping is
  stable from spec §10).

---

## 6. Risks

| Risk | Mitigation |
| ---- | ---------- |
| In-process server scaffold duplication — `tests/connect.rs` re-creates `tests/e2e.rs`'s scaffold | Acceptable for 10.1; a `tests/util/` extraction can come after 10.6 (when 3+ test binaries need the scaffold). |
| brain-server's `tests/e2e.rs` is Linux-only (`#[cfg(target_os = "linux")]`). The SDK test would inherit the same gate | Mirror the cfg-gate. SDK tests that need a running server stay Linux-only; pure-unit tests run host-wide. |
| Wire layer changes between brain-server and brain-sdk-rust | brain-protocol is shared; both crates pin to the same version. No drift possible. |
| Handshake FSM duplication (server has dispatch.rs's pre-AUTH arm) | v1: 50-LOC duplication. A shared `brain-handshake` crate would be a refactor for v2 once the SDK is real. |
| Reusing `next_stream_id` correctly — must allocate odd-numbered IDs for client-initiated streams (spec §03/07 §3) | The placeholder bumps `AtomicU64::fetch_add(2)` starting at 1. Tests assert HELLO is stream 0 (handshake) and the first op uses stream 1. |

---

## 7. What 10.1 explicitly defers

- All sub-tasks 10.2 → 10.13. 10.1 is the foundation only.
- TLS support. Plain TCP works for v1 dev; TLS is a follow-up.
- Auth methods beyond `None`. Token / mTLS land alongside auth
  backends (post-Phase-10).
- Connection pool, idle reaping, keep-alive, reconnect (10.2).
- Retry / backoff (10.3).
- Per-op API methods (10.5).
- Builder pattern for ops (10.5).
- Streaming (10.6).
- Cross-language SDK stubs — out of scope for the Rust SDK
  phase.

---

## 8. Done criteria

- [ ] `crates/brain-sdk-rust/src/{lib,client,config,error,handshake}.rs`
  land with the contents sketched in §3.
- [ ] `Cargo.toml` gains the deps in §4.
- [ ] `tests/connect.rs` passes the two named cases (§5.1, §5.2).
- [ ] All unit tests in §5.3 pass.
- [ ] `just docker-verify` green workspace-wide.
- [ ] Sub-task 10.1 marked `[x]` in `docs/phases/phase-10-sdk-cli.md`.

---

*Implement on approval.*
