# Sub-task 9.9 — Connection layer (Tokio accept + TLS via rustls)

**Reads:**
- `spec/01_system_architecture/04_layers.md` (L1 — connection layer responsibilities).
- `spec/03_wire_protocol/02_transport.md` (TCP + TLS).
- `spec/03_wire_protocol/03_frame_header.md` §1–§3 (32-byte header, CRC).
- `spec/03_wire_protocol/11_validation.md` (server-side frame validation).
- `docs/phases/phase-09-glommio-port.md` §7 (Tokio-side runtime locked).

**Phase doc:** `docs/phases/phase-09-server.md`, this lands as the orientation's **9.9** (was originally numbered 9.3 in the phase doc; the orientation renumbered).

**Done when:** `brain-server` binds a Tokio `TcpListener` to `config.server.listen_addr`, optionally wraps each accepted stream in `tokio-rustls`, spawns a per-connection task that reads `Frame`s from the socket, and parks them at the connection-layer / frame-dispatcher seam (the seam itself lands in 9.10). Connection-task shutdown drains cleanly on a shared signal. macOS host build still works (Tokio is cross-platform; rustls is too).

---

## 1. Scope split with 9.10

The audit and the orientation both put a clean line between "the listener / per-connection plumbing" (9.9) and "frame → opcode → shard dispatch" (9.10). 9.9 ships the *transport*; 9.10 ships the *dispatch*.

| Concern | Lands in |
| ------- | -------- |
| `TcpListener::bind`, accept loop, `TCP_NODELAY`, `SO_KEEPALIVE` | **9.9** |
| Optional `rustls::ServerConfig` from PEM files, TLS handshake | **9.9** |
| Per-connection `tokio::spawn` task, frame read/write loop | **9.9** |
| `Frame::decode_with_max` integration + length-prefix framing | **9.9** |
| HELLO/WELCOME handshake | **9.10** |
| AUTH/AUTH_OK | **9.10** (likely with `AuthMethod::None` in v1 dev) |
| Opcode → shard routing → shard handler → response frame | **9.10** |
| BYE / idle PING / PONG | **9.10** |
| `SUBSCRIBE` fan-out across shards | **9.11** |
| `tokio::sync::broadcast` EventBus relocation | **9.11** |

To keep 9.9 testable end-to-end without forcing a no-op for 9.10, the per-connection task in 9.9 emits an **`ERROR(BadFrame)` response and closes** when any well-formed frame arrives. That gives:

- A real `Frame::encode` round-trip across the socket.
- An assertion-able shape for the integration test ("send `ENCODE`, get `ERROR` back, connection closes cleanly").
- A clean handoff: 9.10 replaces the body of `serve_connection` with the real handshake + dispatch.

---

## 2. The runtime shape

```
brain-server main (Tokio multi-thread)
 ├─► spawn_shard × N  (each one Glommio LocalExecutor on its own OS thread)
 └─► ConnectionListener::serve(listen_addr, tls_cfg, shards, shutdown)
       │
       │ tokio::spawn — accept loop
       │
       ├─► tokio::spawn (per accepted TCP stream)
       │     ├─ optional tokio_rustls::TlsAcceptor::accept(stream).await
       │     ├─ socket.set_nodelay(true) + keepalive
       │     └─ serve_connection(stream, shards, shutdown)
       │           └─ loop: Frame::decode → reply ERROR(BadFrame) → break
       │
       ├─► tokio::spawn (per accepted TCP stream)
       │     └─ …
       │
       └─► shutdown.notified()  → stop accepting; drain in-flight tasks
```

`shards` is `Arc<Vec<ShardHandle>>` (already `Send + Sync` from 9.4/9.7). The connection task doesn't *use* them in 9.9 (dispatch lands in 9.10), but the wiring shape needs to be there so 9.10 plugs in without churning the seam.

### Why `Arc<Vec<…>>` and not a routing struct yet

9.10 introduces a `Router` that wraps the `Vec<ShardHandle>` plus the BLAKE3(agent_id) → shard hash. 9.9 only needs to *carry* the handles to the per-connection task — the router type lives upstream of the dispatcher. Passing `Arc<Vec<ShardHandle>>` keeps 9.9's signature stable across 9.10's introduction.

---

## 3. Module layout

### `crates/brain-server/src/connection.rs` — new (~450 LOC)

Public surface:

```rust
pub struct ConnectionListener {
    listen_addr: SocketAddr,
    tls: Option<Arc<rustls::ServerConfig>>,
    shards: Arc<Vec<ShardHandle>>,
    shutdown: Arc<Notify>,
    limits: ConnectionLimits,
}

pub struct ConnectionLimits {
    pub max_concurrent: usize,         // global cap (default 4096)
    pub max_payload_bytes: u32,        // forwarded to Frame::decode_with_max
    pub read_timeout: Duration,        // per-frame read budget
}

impl ConnectionListener {
    pub fn new(
        listen_addr: SocketAddr,
        tls: Option<Arc<rustls::ServerConfig>>,
        shards: Arc<Vec<ShardHandle>>,
        limits: ConnectionLimits,
        shutdown: Arc<Notify>,
    ) -> Self { ... }

    /// Bind the listener and serve until `shutdown.notified()`.
    /// Returns when the accept loop has exited and all per-connection
    /// tasks have either completed or been aborted on shutdown.
    pub async fn serve(self) -> std::io::Result<()> { ... }
}
```

Internals:
- `serve` binds `TcpListener::bind` (after applying `SO_REUSEADDR`), then loops `tokio::select!` between `listener.accept()` and `self.shutdown.notified()`.
- `accept_one(stream, peer_addr)` — applies TCP options, optionally wraps in TLS, then spawns `serve_connection`.
- `serve_connection<S: AsyncRead + AsyncWrite + Unpin>(stream, shards, limits, shutdown)` — the per-connection task body. 9.9 implementation:
  ```rust
  loop {
      tokio::select! {
          biased;
          _ = shutdown.notified() => break,
          frame = read_one_frame(&mut stream, limits.max_payload_bytes, limits.read_timeout) => {
              match frame {
                  Ok(_) => {
                      write_error_frame(&mut stream, ErrorCode::BadFrame, "9.10 not yet wired").await?;
                      break;
                  }
                  Err(FrameReadError::Eof) => break,
                  Err(FrameReadError::Protocol(_)) => {
                      write_error_frame(&mut stream, ErrorCode::BadFrame, "...").await?;
                      break;
                  }
                  Err(e) => { warn!(error=%e, ...); break; }
              }
          }
      }
  }
  ```

- `read_one_frame` reads exactly `HEADER_SIZE` bytes (looking at `payload_len` in the header), then reads the payload bytes, then calls `Frame::decode_with_max`. Read budget enforced via `tokio::time::timeout`.
- `write_error_frame` builds a wire-ready ERROR frame via `brain_protocol`'s response builder.

### `crates/brain-server/src/tls.rs` — new (~150 LOC)

```rust
pub fn load_server_tls_config(cert_path: &Path, key_path: &Path)
    -> Result<rustls::ServerConfig, TlsLoadError>;

pub enum TlsLoadError {
    Io { path: PathBuf, source: std::io::Error },
    PemParse(String),
    InvalidKey(String),
    Rustls(rustls::Error),
}
```

- Loads PEM cert chain + private key from disk.
- `rustls::ServerConfig::builder()` configured for TLS 1.3 only (per spec §03/02 §2.2).
- ALPN: `b"brain/1"` per spec §03/02 §2.6.
- No mTLS in 9.9; spec §03/02 §2.4 mentions it as opt-in — defer to a follow-up.

### `crates/brain-server/src/main.rs` — wire it up (~80 LOC delta)

After shard spawn, before `tracing::info!("brain-server exiting cleanly")`:

```rust
let shutdown = Arc::new(tokio::sync::Notify::new());
// SIGINT handler (full impl lands in 9.14 — for now a simple ctrl-c trigger).
let shutdown_listener = shutdown.clone();
tokio::spawn(async move {
    let _ = tokio::signal::ctrl_c().await;
    shutdown_listener.notify_waiters();
});

let tls = if cfg.server.tls.enabled {
    let cert = cfg.server.tls.cert.as_ref().expect("config validation: cert required");
    let key = cfg.server.tls.key.as_ref().expect("config validation: key required");
    Some(Arc::new(load_server_tls_config(cert, key)?))
} else {
    None
};
let limits = ConnectionLimits { max_concurrent: 4096, max_payload_bytes: 16 * 1024 * 1024 - 1, read_timeout: Duration::from_secs(30) };
let listener = ConnectionListener::new(cfg.server.listen_addr, tls, shards.clone(), limits, shutdown.clone());

listener.serve().await?;
```

The main function will need to become `async`. Cleanest path: wrap with `#[tokio::main(flavor = "multi_thread")]` (already a dev-dep) — but the binary needs Tokio as a *runtime* dep on Linux now. Add `tokio = { workspace = true, features = ["rt", "rt-multi-thread", "macros", "net", "signal", "time", "io-util"] }` to brain-server's `[target.'cfg(target_os = "linux")'.dependencies]`.

For non-Linux builds the binary stays sync (just config loading), so the `tokio::main` macro is also Linux-gated. Use `#[cfg_attr(target_os = "linux", tokio::main(flavor = "multi_thread"))]` … actually that doesn't compose nicely. Simpler: keep `main` sync, build the runtime by hand only on Linux:

```rust
#[cfg(target_os = "linux")]
fn main() -> ExitCode {
    // parse args, load cfg, build runtime, then runtime.block_on(run(cfg))
}

#[cfg(not(target_os = "linux"))]
fn main() -> ExitCode {
    // existing sync flow, just config + print
}
```

### `crates/brain-server/Cargo.toml`

Add to the Linux-only block:
```toml
tokio = { workspace = true, features = ["rt", "rt-multi-thread", "macros", "net", "signal", "time", "io-util"] }
tokio-rustls = { workspace = true }
rustls = { workspace = true }
rustls-pemfile = { workspace = true }
```

Workspace `Cargo.toml`:
```toml
tokio-rustls = "0.26"
rustls = { version = "0.23", default-features = false, features = ["std", "logging", "tls12"] }
# (actually we want TLS 1.3 only; `aws-lc-rs` or `ring` provider per build env)
rustls-pemfile = "2"
```

**Risk: rustls provider.** rustls 0.23 dropped the built-in provider; callers pick `aws_lc_rs` or `ring` and install it. Defer the provider choice to the `tls.rs` module so the binary explicitly installs `rustls::crypto::aws_lc_rs::default_provider().install_default()` at startup. (Or `ring` if `aws-lc-rs` adds build-time friction in the container — check during impl.)

### `crates/brain-server/tests/connection.rs` — new (~250 LOC)

Tokio-side integration tests. No Glommio required — the connection task in 9.9 doesn't touch shards.

1. **`bind_and_accept_succeeds`** — bind to `127.0.0.1:0`, connect from a client task, expect TCP_NODELAY observed, then close.
2. **`server_rejects_well_formed_frame_with_error`** — connect, send a valid `ENCODE` frame, read back an `ERROR(BadFrame)` frame, observe connection close.
3. **`server_rejects_bad_magic`** — write 32 bytes of junk, expect `ERROR(BadFrame)` and close.
4. **`server_honors_read_timeout`** — connect, send half a header, sleep past `read_timeout`, expect connection close.
5. **`shutdown_signal_stops_accept_and_drains_inflight`** — connect, then `shutdown.notify_waiters()`, expect `listener.serve()` to return and the connection task to exit.
6. **`tls_round_trip_smoke`** — bind with rustls cert+key (self-signed via `rcgen` dev-dep), connect with `tokio-rustls` client side, verify the TLS handshake completes and one frame round-trips. (Gated `#[cfg_attr(not(feature = "tls"), ignore)]`-style if rustls dep makes CI heavy — likely just always run.)

### `crates/brain-server/src/main.rs` test helpers

The cfg-split between Linux (async main) and non-Linux (sync main) means the existing arg-parse tests stay as-is. No churn there.

---

## 4. Spec compliance details

### TCP options (spec §03/02 §1.2)

- `TCP_NODELAY` — `socket.set_nodelay(true)?` on the accepted stream.
- `SO_KEEPALIVE` — `socket2::TcpKeepalive` configured with idle=75s, interval=15s, count=9. Apply via the unsafe `socket2::Socket::from(stream)` cycle, or pull through `tokio::net::TcpSocket` builder for the listener side.
- `SO_REUSEADDR` — on the listener socket via `TcpSocket::set_reuseaddr(true)`.

### TLS 1.3 only (spec §03/02 §2.2)

```rust
let cfg = rustls::ServerConfig::builder()
    .with_protocol_versions(&[&rustls::version::TLS13])?
    .with_no_client_auth()
    .with_single_cert(cert_chain, private_key)?;
```

### ALPN `"brain/1"` (spec §03/02 §2.6)

```rust
cfg.alpn_protocols.push(b"brain/1".to_vec());
```

### Frame max size

`limits.max_payload_bytes` defaults to `(1 << 24) - 1` (16 MiB - 1, the spec hard cap), but operators may want to tighten via config. Add `server.max_payload_bytes` in a follow-up; 9.9 just exposes the field.

### Idle timeout (spec §03/02 §6.1)

Defer to 9.10 — needs application-level PING frames, which need the frame dispatcher. 9.9's `read_timeout` is a *per-frame* watchdog, not the application idle-timer.

---

## 5. Open questions (surface only)

1. **rustls crypto provider.** `aws-lc-rs` vs `ring`. `aws-lc-rs` is the rustls default but requires a C toolchain (cmake + clang) at build time. `ring` is pure-Rust + Go-Rust hybrid (no C cmake). The Brain Linux container already has clang for candle, so `aws-lc-rs` should "just work" — verify during impl. If it doesn't, fall back to `ring`.

2. **Self-signed cert generation for the smoke test.** `rcgen` is the obvious choice (pure-Rust). Add as a dev-dep — risk is minimal.

3. **Connection-limit enforcement.** A simple `Arc<Semaphore>(max_concurrent)` around `tokio::spawn` is enough; if it can't be acquired immediately, send `ERROR(ServerBusy)` and close. Spec §03/02 §1.3's per-IP + per-agent caps are 9.13 (admin-side, observability-anchored).

4. **What to log on connect/disconnect.** Suggested: `tracing::info!(peer = %addr, tls = ..., "connection accepted")` and `tracing::debug!(peer = %addr, "connection closed")`. Avoid info-level on every close (noisy under load).

5. **`Frame::decode_with_max` vs streaming reads.** The current `Frame::decode` API takes a `&[u8]` slice. We need to *read into a buffer*, which means we need a small async helper:
   ```rust
   async fn read_one_frame<S: AsyncReadExt + Unpin>(s: &mut S, max: u32, timeout: Duration)
       -> Result<Frame, FrameReadError>
   ```
   This is internal to `connection.rs`. No `brain-protocol` surface changes — we read 32 bytes, peek `payload_len`, read N more bytes, call `Frame::decode_with_max` on the joined buffer. Possible perf win in v2: streaming decode that doesn't double-buffer. Out of scope here.

---

## 6. Sizing

| File | Action | LOC |
| ---- | ------ | --- |
| `crates/brain-server/src/connection.rs` | new | ~450 |
| `crates/brain-server/src/tls.rs` | new | ~150 |
| `crates/brain-server/src/main.rs` | wire-up + Linux/non-Linux split | ~80 (delta) |
| `crates/brain-server/Cargo.toml` | tokio + tokio-rustls + rustls deps | ~10 |
| `Cargo.toml` (workspace) | new dep declarations | ~6 |
| `crates/brain-server/tests/connection.rs` | new | ~250 |

Total: ~950 LOC. Single commit.

---

## 7. Risks

| Risk | Mitigation |
| ---- | ---------- |
| rustls crypto provider doesn't compile in the dev container | Fall back to `ring`. Decision happens at first `cargo check`. |
| TLS smoke test flakes under load (rcgen + self-signed cert verification) | Use `ServerCertVerifier` impl that trusts the test cert directly. Pure-Rust, no system-trust-store coupling. |
| `tokio::main` and the cfg-split between Linux/non-Linux gets ugly | Manual `tokio::runtime::Builder::new_multi_thread().build()?.block_on(...)` on Linux; keep `fn main() -> ExitCode` sync on non-Linux. ~10 LOC, no macro. |
| Connection task leaks on graceful shutdown | The accept loop's `tokio::select!` drops the `Notify`-listener future when the listener exits; per-connection tasks see `shutdown.notified()` and return Ok(()). Test 5 (`shutdown_signal_stops_accept_and_drains_inflight`) covers this. |
| `Frame::decode_with_max` allocates per frame | Acceptable for 9.9. v2 may add an arena-backed decoder. Out of scope. |

---

## 8. Done criteria

- [ ] `ConnectionListener` + `serve` ship in `crates/brain-server/src/connection.rs`.
- [ ] `load_server_tls_config` ships in `crates/brain-server/src/tls.rs`.
- [ ] `brain-server`'s `main` on Linux: parses args, loads config, spawns shards, runs `ConnectionListener::serve` inside a Tokio runtime; handles `ctrl-c` for graceful shutdown.
- [ ] `brain-server` on non-Linux: unchanged (sync main, prints "Phase 9 stub" warning).
- [ ] 6 integration tests pass (bind, accept, reject-frame, bad-magic, read-timeout, shutdown, tls-smoke).
- [ ] `just docker-verify` green workspace-wide.
- [ ] Phase doc 9.9 (was 9.3) marked `[x]`.
- [ ] Audit doc §7 status row flipped to **done** for the Tokio side.

---

## 9. What 9.9 explicitly defers

- **Handshake (HELLO/WELCOME/AUTH/AUTH_OK)** — 9.10.
- **Real opcode → shard dispatch** — 9.10.
- **BYE / idle PING / PONG** — 9.10.
- **`SUBSCRIBE` fan-out** — 9.11.
- **mTLS** — follow-up; spec §03/02 §2.4 marks it opt-in.
- **Per-IP + per-agent connection limits** — 9.13 (admin/observability anchor).
- **SIGTERM + drain timer** — 9.14 (graceful shutdown owns the full lifecycle).

---

*Implement on approval.*
