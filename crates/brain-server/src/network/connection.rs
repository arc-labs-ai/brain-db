//! Connection layer — Tokio TCP accept loop with optional rustls TLS
//! (sub-task 9.9). Spec §01/04 (L1), §03/02 (transport).
//!
//! ## What 9.9 ships
//!
//! - `TcpListener::bind` on `config.server.listen_addr` with
//!   `SO_REUSEADDR`.
//! - Optional `tokio_rustls::TlsAcceptor` wrap on accepted streams.
//! - Per-connection task: applies `TCP_NODELAY` + `SO_KEEPALIVE`,
//!   reads one frame at a time with a per-frame read timeout, validates
//!   with [`brain_protocol::Frame::decode_with_max`], and (for now)
//!   replies `ERROR(BadFrame)` then closes.
//! - Graceful shutdown via a `watch::channel`-based [`ShutdownSignal`]
//!   shared with `main`. (Switched off `tokio::sync::Notify` to avoid
//!   the "wake lost between loop iterations" race.)
//!
//! ## What 9.10 will plug in
//!
//! The body of [`serve_connection`] becomes the real handshake →
//! AUTH → dispatch loop. The frame I/O helpers and the shutdown wiring
//! stay as they are; only the inner match changes.
//!
//! ## What stays out of 9.9
//!
//! - HELLO/WELCOME/AUTH/AUTH_OK handshake — 9.10.
//! - Real opcode → shard routing — 9.10.
//! - Idle PING/PONG, BYE handling — 9.10.
//! - Per-IP / per-agent connection limits — 9.13.
//! - mTLS — follow-up; spec §03/02 §2.4 marks opt-in.

#![cfg(target_os = "linux")]

use std::io;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::error::{ErrorCategory, ErrorCode};
use brain_protocol::opcode::Opcode;
use brain_protocol::response::{ErrorCategoryWire, ErrorCodeWire, ErrorResponse, ResponseBody};
use brain_protocol::{Frame, HEADER_SIZE, MAX_PAYLOAD_BYTES};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{split, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::watch;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn, Instrument as _};

pub use crate::dispatch::Topology;

use crate::dispatch::{
    build_server_ping_frame, dispatch_frame, run_op_dispatch, Action, CancelSubscribe, ConnState,
    IdleTimer, SubscribeStart, Tick,
};
use crate::subscribe::{
    build_cancel_stream_ack_frame, build_unsubscribe_response_frame, ShardEventHub,
    SubscriptionRegistry,
};

// ---------------------------------------------------------------------------
// Shutdown signal
// ---------------------------------------------------------------------------

/// Edge-triggered shutdown channel. `signal()` flips the value to
/// `true`; every receiver (the listener + every per-connection task)
/// observes the flip either immediately via [`Self::is_signalled`] or
/// asynchronously via [`Self::recv`]. Built on `tokio::sync::watch` so
/// late observers don't miss the edge — unlike `Notify::notify_waiters`
/// which only wakes currently-parked tasks.
#[derive(Clone)]
pub struct ShutdownSignal(watch::Receiver<bool>);

/// Producer half: hold this in `main` (or the test scaffold). Dropping
/// it has no effect on receivers — call [`Self::signal`] explicitly.
pub struct ShutdownTrigger(watch::Sender<bool>);

impl ShutdownSignal {
    /// Create a fresh signal pair (un-signalled by default).
    #[must_use]
    pub fn channel() -> (ShutdownTrigger, ShutdownSignal) {
        let (tx, rx) = watch::channel(false);
        (ShutdownTrigger(tx), ShutdownSignal(rx))
    }

    /// Has the trigger fired yet? Non-blocking.
    pub fn is_signalled(&self) -> bool {
        *self.0.borrow()
    }

    /// Resolve when the trigger fires. Returns immediately if it has
    /// already fired (i.e. value is already `true` and the receiver
    /// hasn't acknowledged it).
    pub async fn recv(&mut self) {
        if self.is_signalled() {
            return;
        }
        // `changed()` returns `Err(_)` only when the sender drops; for
        // the connection layer that's equivalent to shutdown.
        let _ = self.0.changed().await;
    }
}

impl ShutdownTrigger {
    pub fn signal(&self) {
        let _ = self.0.send(true);
    }
}

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Per-listener tuning knobs.
#[derive(Clone, Debug)]
pub struct ConnectionLimits {
    /// Maximum payload bytes accepted by `Frame::decode_with_max`. Defaults
    /// to the 24-bit spec hard cap (16 MiB - 1).
    pub max_payload_bytes: u32,
    /// Per-frame read budget. Bytes received before this deadline elapses
    /// are kept; the deadline is enforced per `read_one_frame` call. A
    /// connection that goes silent mid-frame is closed.
    pub read_timeout: Duration,
    /// Spec §03/06 §6.3 — interval before AUTH must arrive after WELCOME.
    pub auth_timeout: Duration,
    /// Spec §03/02 §6.1 — idle window before the server emits SERVER_PING.
    pub idle_timeout: Duration,
    /// Spec §03/02 §6.1 — window for CLIENT_PONG to arrive after SERVER_PING.
    pub ping_timeout: Duration,
    /// Outgoing-frame channel capacity. Bounds memory under sustained
    /// load; if the writer can't keep up, sub-tasks back-pressure on
    /// `send_async` and the read loop naturally slows down.
    pub outgoing_capacity: usize,
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
            read_timeout: Duration::from_secs(30),
            auth_timeout: Duration::from_secs(30),
            idle_timeout: Duration::from_secs(300),
            ping_timeout: Duration::from_secs(30),
            outgoing_capacity: 256,
        }
    }
}

/// Live connection counters surfaced via the admin `/metrics`
/// endpoint. Extended in 12.7 (closed-by-reason + frame send/recv
/// counters) and in F-7 (frame_size_bytes histograms after the
/// unit-agnostic `Histogram` refactor).
pub struct ConnectionMetrics {
    pub active: AtomicU64,
    pub total: AtomicU64,
    /// Per-reason close counters; indexed by [`CloseReason::idx`].
    pub closed_by_reason: [AtomicU64; CloseReason::COUNT],
    pub frame_send_total: AtomicU64,
    pub frame_recv_total: AtomicU64,
    /// F-7 (`docs/spec-audit/fix-plan.md`): raw-mode histograms of
    /// outbound / inbound frame size in bytes. `Histogram::new` with
    /// [`FRAME_BYTES_BUCKETS`] gives an exact `_sum`.
    pub frame_send_bytes: crate::metrics::histogram::Histogram,
    pub frame_recv_bytes: crate::metrics::histogram::Histogram,
}

/// Frame-size histogram bucket bounds in **bytes**. Covers tiny
/// control frames (PONG, BYE, ~32 B) through the spec §03 hard
/// 16 MiB payload max, with extra resolution in the 1-64 KiB band
/// where most request/response payloads land.
pub const FRAME_BYTES_BUCKETS: &[f64] = &[
    64.0,
    256.0,
    1_024.0,
    4_096.0,
    16_384.0,
    65_536.0,
    262_144.0,
    1_048_576.0,
    4_194_304.0,
    16_777_216.0,
];

impl Default for ConnectionMetrics {
    fn default() -> Self {
        Self {
            active: AtomicU64::new(0),
            total: AtomicU64::new(0),
            closed_by_reason: Default::default(),
            frame_send_total: AtomicU64::new(0),
            frame_recv_total: AtomicU64::new(0),
            frame_send_bytes: crate::metrics::histogram::Histogram::new(FRAME_BYTES_BUCKETS),
            frame_recv_bytes: crate::metrics::histogram::Histogram::new(FRAME_BYTES_BUCKETS),
        }
    }
}

/// Stable close-reason set spec §14/01 §9 expects on
/// `brain_connections_closed_total{reason=}`. Order is the
/// label-emission order; the indexed array in
/// [`ConnectionMetrics::closed_by_reason`] uses this same order.
#[derive(Clone, Copy, Debug)]
pub enum CloseReason {
    Bye,
    ProtocolError,
    Timeout,
    Eof,
    Fatal,
}

impl CloseReason {
    pub const COUNT: usize = 5;

    /// Lookup label used by `brain_connections_closed_total{reason=}`.
    /// Reserved for diagnostics that report the close cause without
    /// going through the metric registry; the exposition path uses
    /// [`CLOSE_REASONS`] indexed by [`Self::idx`] instead.
    #[must_use]
    #[allow(dead_code)]
    pub fn label(self) -> &'static str {
        match self {
            CloseReason::Bye => "bye",
            CloseReason::ProtocolError => "protocol_error",
            CloseReason::Timeout => "timeout",
            CloseReason::Eof => "eof",
            CloseReason::Fatal => "fatal",
        }
    }

    #[must_use]
    pub fn idx(self) -> usize {
        match self {
            CloseReason::Bye => 0,
            CloseReason::ProtocolError => 1,
            CloseReason::Timeout => 2,
            CloseReason::Eof => 3,
            CloseReason::Fatal => 4,
        }
    }
}

/// Label strings for `brain_connections_closed_total{reason=}` in
/// the same order as [`ConnectionMetrics::closed_by_reason`].
pub const CLOSE_REASONS: [&str; CloseReason::COUNT] =
    ["bye", "protocol_error", "timeout", "eof", "fatal"];

impl ConnectionMetrics {
    /// Bump the per-reason close counter.
    pub fn record_close(&self, reason: CloseReason) {
        self.closed_by_reason[reason.idx()].fetch_add(1, Ordering::Relaxed);
    }

    /// Bump the outbound frame counter and observe the frame size.
    pub fn observe_send(&self, bytes: usize) {
        self.frame_send_total.fetch_add(1, Ordering::Relaxed);
        self.frame_send_bytes.observe(bytes as f64);
    }

    /// Bump the inbound frame counter and observe the frame size.
    pub fn observe_recv(&self, bytes: usize) {
        self.frame_recv_total.fetch_add(1, Ordering::Relaxed);
        self.frame_recv_bytes.observe(bytes as f64);
    }
}

/// Decrement `active` when this guard drops. Survives panic and
/// early-return paths inside the per-connection task.
struct ConnectionGuard {
    metrics: Arc<ConnectionMetrics>,
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.metrics.active.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Unbound listener config. Call [`ConnectionListener::bind`] to
/// produce a [`BoundConnectionListener`] (which exposes
/// [`BoundConnectionListener::local_addr`] before [`Self::serve`]).
pub struct ConnectionListener {
    listen_addr: SocketAddr,
    tls: Option<Arc<ServerConfig>>,
    topology: Topology,
    /// Cross-shard event hub (sub-task 9.11). Built once per listener
    /// at construction time; spawns one bridge task per shard that
    /// drains the shard's per-process flume Receiver into a
    /// `broadcast::Sender`. Per-connection `SubscriptionRegistry`s
    /// subscribe to the right shard's broadcast.
    event_hub: ShardEventHub,
    /// Live counters surfaced by the admin server (sub-task 9.13).
    metrics: Arc<ConnectionMetrics>,
    limits: ConnectionLimits,
    shutdown: ShutdownSignal,
}

/// Listener that has already opened its TCP socket. The address is
/// observable via [`Self::local_addr`] before [`Self::serve`] is awaited.
pub struct BoundConnectionListener {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: Option<Arc<ServerConfig>>,
    topology: Topology,
    event_hub: ShardEventHub,
    metrics: Arc<ConnectionMetrics>,
    limits: ConnectionLimits,
    shutdown: ShutdownSignal,
}

impl ConnectionListener {
    pub fn new(
        listen_addr: SocketAddr,
        tls: Option<Arc<ServerConfig>>,
        topology: Topology,
        metrics: Arc<ConnectionMetrics>,
        limits: ConnectionLimits,
        shutdown: ShutdownSignal,
    ) -> Self {
        // Spawn the per-shard event bridge tasks now so every
        // connection sees the same hub. The bridges live for the
        // lifetime of the process (or until the shards' flume
        // Receivers return Err — i.e., shard shutdown).
        let event_hub = ShardEventHub::spawn(&topology.shards);
        Self {
            listen_addr,
            tls,
            topology,
            event_hub,
            metrics,
            limits,
            shutdown,
        }
    }

    /// Bind the TCP socket. The returned [`BoundConnectionListener`]
    /// exposes the actual bound address (useful when `listen_addr`
    /// specifies port 0 for ephemeral binding in tests).
    pub fn bind(self) -> io::Result<BoundConnectionListener> {
        let listener = bind_listener(self.listen_addr)?;
        let local_addr = listener.local_addr()?;
        info!(
            addr = %local_addr,
            tls = self.tls.is_some(),
            "connection listener bound"
        );
        Ok(BoundConnectionListener {
            listener,
            local_addr,
            tls: self.tls,
            topology: self.topology,
            event_hub: self.event_hub,
            metrics: self.metrics,
            limits: self.limits,
            shutdown: self.shutdown,
        })
    }
}

impl BoundConnectionListener {
    /// The address the socket is actually bound to. With a `:0` port in
    /// `listen_addr`, this is the kernel-assigned ephemeral port.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Serve until the shutdown signal fires.
    ///
    /// Returns once the accept loop has exited. Per-connection tasks
    /// that were already running are NOT awaited here — they observe
    /// the same `shutdown` notify and unwind on their own. 9.14 layers
    /// a JoinSet-based drain over this.
    pub async fn serve(mut self) -> io::Result<SocketAddr> {
        let local_addr = self.local_addr;
        info!(addr = %local_addr, "connection listener accepting");

        let acceptor = self.tls.clone().map(TlsAcceptor::from);

        loop {
            tokio::select! {
                biased;
                () = self.shutdown.recv() => {
                    info!(addr = %local_addr, "connection listener shutdown signalled");
                    return Ok(local_addr);
                }
                accepted = self.listener.accept() => {
                    let (stream, peer) = match accepted {
                        Ok(p) => p,
                        Err(e) => {
                            warn!(error = %e, "accept failed");
                            continue;
                        }
                    };
                    if let Err(e) = configure_tcp(&stream) {
                        warn!(peer = %peer, error = %e, "TCP option setup failed");
                    }
                    let acceptor = acceptor.clone();
                    let shutdown = self.shutdown.clone();
                    let limits = self.limits.clone();
                    let topology = self.topology.clone();
                    let event_hub = self.event_hub.clone();
                    let metrics = self.metrics.clone();
                    // Counter bookkeeping (sub-task 9.13). `_guard`
                    // decrements `active` on drop — handles every
                    // exit path including TLS handshake failure.
                    metrics.total.fetch_add(1, Ordering::Relaxed);
                    metrics.active.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move {
                        let _guard = ConnectionGuard {
                            metrics: metrics.clone(),
                        };
                        let result = match acceptor {
                            Some(acceptor) => match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    serve_connection(
                                        tls_stream, topology, event_hub, limits, shutdown,
                                        metrics.clone(),
                                    )
                                    .await
                                }
                                Err(e) => {
                                    debug!(peer = %peer, error = %e, "TLS handshake failed");
                                    return;
                                }
                            },
                            None => {
                                serve_connection(
                                    stream, topology, event_hub, limits, shutdown,
                                    metrics.clone(),
                                )
                                .await
                            }
                        };
                        if let Err(e) = result {
                            debug!(peer = %peer, error = %e, "connection ended");
                        }
                    });
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Per-connection task
// ---------------------------------------------------------------------------

/// One connection's lifetime (sub-task 9.10). Splits the stream into
/// reader + writer halves and runs three loops:
///
/// 1. **Reader** — pulls frames from the socket, decides via
///    [`crate::dispatch::dispatch_frame`] whether to handle inline,
///    spawn a per-op dispatch sub-task, or close. Drives the idle /
///    auth timer in the same `select!`.
/// 2. **Writer** — drains a per-connection `flume` queue and writes
///    bytes to the socket.
/// 3. **Op sub-tasks** — `tokio::spawn`-ed per data-plane request;
///    await the shard reply, encode the response frame, push it into
///    the writer queue.
///
/// The Tokio↔Glommio boundary lives in `ShardHandle::dispatch_op`
/// (which sends through a `flume::Sender<ShardRequest>`). The
/// per-connection task is fully Tokio-side; only the shard handler
/// runs inside Glommio.
pub(crate) async fn serve_connection<S>(
    stream: S,
    topology: Topology,
    event_hub: ShardEventHub,
    limits: ConnectionLimits,
    shutdown: ShutdownSignal,
    metrics: Arc<ConnectionMetrics>,
) -> io::Result<()>
where
    S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
{
    let (mut read_half, write_half) = split(stream);
    let (frame_tx, frame_rx) = flume::bounded::<OutgoingFrame>(limits.outgoing_capacity);
    let writer_metrics = metrics.clone();
    let writer = tokio::spawn(writer_loop(write_half, frame_rx, writer_metrics));

    // Sub-task 9.11: each connection gets its own SubscriptionRegistry.
    // It reuses the listener-wide `ShardEventHub` to subscribe to per-
    // shard broadcasts.
    let subscriptions = Arc::new(SubscriptionRegistry::new(event_hub));

    let result = receiver_loop(
        &mut read_half,
        &topology,
        &limits,
        shutdown,
        frame_tx.clone(),
        subscriptions,
        metrics.clone(),
    )
    .await;

    // Dropping the last frame_tx closes the writer queue → writer_loop
    // returns. (The op-dispatch sub-tasks each cloned the sender; we
    // can't await them here because we'd block shutdown. They drop
    // their senders as they complete, and the writer task awaits its
    // own channel close cooperatively.)
    drop(frame_tx);
    let _ = writer.await;
    result
}

// One outgoing frame: bytes pre-encoded, with an optional "close after
// this frame" hint that lets the writer loop tear down on the right
// frame (BYE, fatal ERROR).
pub(crate) struct OutgoingFrame {
    pub(crate) bytes: Vec<u8>,
    pub(crate) close_after: bool,
}

async fn writer_loop<W>(
    mut write: W,
    rx: flume::Receiver<OutgoingFrame>,
    metrics: Arc<ConnectionMetrics>,
) where
    W: AsyncWrite + Unpin,
{
    while let Ok(out) = rx.recv_async().await {
        if let Err(e) = write.write_all(&out.bytes).await {
            debug!(error = %e, "writer flush failed");
            break;
        }
        if let Err(e) = write.flush().await {
            debug!(error = %e, "writer flush failed");
            break;
        }
        metrics.observe_send(out.bytes.len());
        if out.close_after {
            break;
        }
    }
}

async fn receiver_loop<R>(
    read: &mut R,
    topology: &Topology,
    limits: &ConnectionLimits,
    mut shutdown: ShutdownSignal,
    frame_tx: flume::Sender<OutgoingFrame>,
    subscriptions: Arc<SubscriptionRegistry>,
    metrics: Arc<ConnectionMetrics>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    let mut state = ConnState::new();
    let mut idle = IdleTimer::new(limits.idle_timeout, limits.ping_timeout);
    let mut handshake_deadline = Some(tokio::time::Instant::now() + limits.auth_timeout);

    loop {
        // Compute the next deadline: handshake timeout while pre-AUTH,
        // idle / ping timeout post-AUTH.
        let next_deadline = match handshake_deadline {
            Some(d) => d,
            None => idle.next_deadline(),
        };

        tokio::select! {
            biased;
            () = shutdown.recv() => return Ok(()),
            _ = tokio::time::sleep_until(next_deadline) => {
                if handshake_deadline.is_some() {
                    // Spec §03/06 §6.3 — auth timeout before AUTH_OK.
                    let frame = build_close_error_frame(
                        ErrorCode::Unauthenticated,
                        "handshake timeout (no AUTH within auth_timeout)",
                    );
                    let _ = frame_tx.send_async(OutgoingFrame {
                        bytes: frame.encode(),
                        close_after: true,
                    }).await;
                    return Ok(());
                }
                match idle.fire() {
                    Tick::SendPing => {
                        let frame = build_server_ping_frame();
                        if frame_tx.send_async(OutgoingFrame {
                            bytes: frame.encode(),
                            close_after: false,
                        }).await.is_err() {
                            return Ok(());
                        }
                    }
                    Tick::Close => {
                        // SERVER_PING went unanswered past ping_timeout.
                        metrics.record_close(CloseReason::Timeout);
                        return Ok(());
                    }
                }
            }
            result = read_one_frame(read, limits.max_payload_bytes, limits.read_timeout) => {
                idle.on_frame_received();
                match result {
                    Ok(frame) => {
                        // Wire size = fixed header + payload bytes.
                        metrics.observe_recv(HEADER_SIZE + frame.payload.len());
                        let action = dispatch_frame(frame, &mut state, topology);
                        // Once handshake is complete, drop the auth deadline.
                        if matches!(
                            state.phase,
                            crate::dispatch::ConnPhase::Established { .. }
                        ) {
                            handshake_deadline = None;
                        }
                        match action {
                            Action::Inline(frame) => {
                                if frame_tx.send_async(OutgoingFrame {
                                    bytes: frame.encode(),
                                    close_after: false,
                                }).await.is_err() {
                                    return Ok(());
                                }
                            }
                            Action::OpDispatch(op) => {
                                let shards = topology.shards.clone();
                                let request_metrics = topology.request_metrics.clone();
                                let tx = frame_tx.clone();
                                let op_idx = crate::metrics::request::op_index(&op.req);
                                let op_label = op_idx
                                    .and_then(|i| crate::metrics::request::OP_LABELS.get(i))
                                    .copied()
                                    .unwrap_or("unknown");
                                let stream_id = op.stream_id;
                                let target_shard = op.target_shard;
                                // 12.3 — request-level span. Spec §14/03 §3 instruments each
                                // request; child spans inside the shard (brain.encode →
                                // brain.embed → brain.hnsw.insert) attach to this parent.
                                let span = tracing::info_span!(
                                    "brain.request",
                                    op = op_label,
                                    stream_id,
                                    target_shard,
                                );
                                tokio::spawn(
                                    async move {
                                        let timer = op_idx.map(|idx| {
                                            crate::metrics::request::RequestTimer::start(
                                                request_metrics.clone(),
                                                idx,
                                            )
                                        });
                                        let frame = run_op_dispatch(op, shards).await;
                                        if let Some(timer) = timer {
                                            let status = response_status(&frame);
                                            timer.record(status);
                                        }
                                        let _ = tx.send_async(OutgoingFrame {
                                            bytes: frame.encode(),
                                            close_after: false,
                                        }).await;
                                    }
                                    .instrument(span),
                                );
                            }
                            Action::Subscribe(start) => {
                                handle_subscribe_start(
                                    start,
                                    &subscriptions,
                                    &frame_tx,
                                )
                                .await;
                            }
                            Action::CancelSubscribe(c) => {
                                handle_cancel_subscribe(
                                    c,
                                    &subscriptions,
                                    &frame_tx,
                                )
                                .await;
                            }
                            Action::CloseWith(frame) => {
                                let _ = frame_tx.send_async(OutgoingFrame {
                                    bytes: frame.encode(),
                                    close_after: true,
                                }).await;
                                // BYE handler routes here; other CloseWith
                                // arms (BadOpcode etc.) are protocol errors.
                                let reason = if frame.header.opcode_u16() == Opcode::Bye.as_u16() {
                                    CloseReason::Bye
                                } else {
                                    CloseReason::ProtocolError
                                };
                                metrics.record_close(reason);
                                return Ok(());
                            }
                            Action::Close => {
                                metrics.record_close(CloseReason::Bye);
                                return Ok(());
                            }
                            Action::Nothing => {}
                        }
                    }
                    Err(FrameReadError::Eof) => {
                        metrics.record_close(CloseReason::Eof);
                        return Ok(());
                    }
                    Err(FrameReadError::Protocol(code, category, detail)) => {
                        let frame = build_close_error_frame_with_category(code, category, &detail);
                        let _ = frame_tx.send_async(OutgoingFrame {
                            bytes: frame.encode(),
                            close_after: true,
                        }).await;
                        metrics.record_close(CloseReason::ProtocolError);
                        return Ok(());
                    }
                    Err(FrameReadError::Timeout) => {
                        // Per-frame read budget expired. Close quietly;
                        // the idle/SERVER_PING path is for application-
                        // level keepalive.
                        metrics.record_close(CloseReason::Timeout);
                        return Ok(());
                    }
                    Err(FrameReadError::Io(e)) => {
                        metrics.record_close(CloseReason::Fatal);
                        return Err(e);
                    }
                }
            }
        }
    }
}

fn build_close_error_frame(code: ErrorCode, message: &str) -> Frame {
    build_close_error_frame_with_category(code, code.category(), message)
}

/// Map a finished response frame to a [`request::Status`] for
/// metrics. Looks at the wire opcode only — `0xFF` (Error) becomes
/// `Status::Error`; anything else is `Status::Success`. The
/// in-flight gauge / drop-without-record path handles `Timeout`.
fn response_status(frame: &Frame) -> crate::metrics::request::Status {
    if frame.header.opcode_u16() == Opcode::Error.as_u16() {
        crate::metrics::request::Status::Error
    } else {
        crate::metrics::request::Status::Success
    }
}

fn build_close_error_frame_with_category(
    code: ErrorCode,
    category: ErrorCategory,
    message: &str,
) -> Frame {
    let body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(code),
        category: ErrorCategoryWire::from(category),
        message: message.to_owned(),
        details: None,
        retry_after_ms: None,
    });
    Frame::new(Opcode::Error.as_u16(), 0, 0, body.encode())
}

// ---------------------------------------------------------------------------
// SUBSCRIBE / UNSUBSCRIBE / CANCEL_STREAM helpers (sub-task 9.11)
// ---------------------------------------------------------------------------

async fn handle_subscribe_start(
    start: SubscribeStart,
    subscriptions: &Arc<SubscriptionRegistry>,
    frame_tx: &flume::Sender<OutgoingFrame>,
) {
    let SubscribeStart {
        stream_id,
        req,
        target_shard,
    } = start;
    match subscriptions.start(stream_id, target_shard, &req, frame_tx.clone()) {
        Ok(_) => {
            // Subscription established. Per-sub task is running; it
            // will start emitting SUBSCRIBE_EVENT frames as events
            // arrive. 9.11 doesn't send a synchronous opener frame
            // (the wire protocol doesn't require one); the client
            // observes the first event when it lands.
        }
        Err(e) => {
            let frame = e.to_error_frame(stream_id);
            let _ = frame_tx
                .send_async(OutgoingFrame {
                    bytes: frame.encode(),
                    close_after: false,
                })
                .await;
        }
    }
}

async fn handle_cancel_subscribe(
    c: CancelSubscribe,
    subscriptions: &Arc<SubscriptionRegistry>,
    frame_tx: &flume::Sender<OutgoingFrame>,
) {
    let (request_stream_id, target_stream_id, reply_frame) = match c {
        CancelSubscribe::Unsubscribe {
            request_stream_id,
            req,
        } => {
            let target = req.target_stream_id;
            let final_lsn = subscriptions.cancel(target).unwrap_or(0);
            let frame = build_unsubscribe_response_frame(request_stream_id, &req, final_lsn);
            (request_stream_id, target, frame)
        }
        CancelSubscribe::CancelStream {
            request_stream_id,
            req,
        } => {
            let target = req.target_stream_id;
            subscriptions.cancel(target);
            let now_ns = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
                .unwrap_or(0);
            let frame = build_cancel_stream_ack_frame(request_stream_id, &req, now_ns);
            (request_stream_id, target, frame)
        }
    };
    let _ = (request_stream_id, target_stream_id);
    let _ = frame_tx
        .send_async(OutgoingFrame {
            bytes: reply_frame.encode(),
            close_after: false,
        })
        .await;
}

// ---------------------------------------------------------------------------
// Frame I/O helpers
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
enum FrameReadError {
    #[error("connection closed by peer")]
    Eof,
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("read timed out")]
    Timeout,
    #[error("protocol error: {2}")]
    Protocol(ErrorCode, ErrorCategory, String),
}

async fn read_one_frame<S>(
    stream: &mut S,
    max_payload_bytes: u32,
    timeout: Duration,
) -> Result<Frame, FrameReadError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    tokio::time::timeout(timeout, read_one_frame_inner(stream, max_payload_bytes))
        .await
        .map_err(|_| FrameReadError::Timeout)?
}

async fn read_one_frame_inner<S>(
    stream: &mut S,
    max_payload_bytes: u32,
) -> Result<Frame, FrameReadError>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header_buf = [0u8; HEADER_SIZE];
    match stream.read_exact(&mut header_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Err(FrameReadError::Eof),
        Err(e) => return Err(FrameReadError::Io(e)),
    }

    // Peek payload_len *without* validating yet — we want to bound the
    // allocation before reading from the wire. `decode_with_max` re-
    // validates the header (including magic / CRC) once the full frame
    // bytes are present.
    let payload_len_be = [header_buf[16], header_buf[17], header_buf[18]];
    let payload_len =
        u32::from_be_bytes([0, payload_len_be[0], payload_len_be[1], payload_len_be[2]]);
    if payload_len > max_payload_bytes {
        return Err(FrameReadError::Protocol(
            ErrorCode::BadFrame,
            ErrorCategory::Protocol,
            format!("payload_len {payload_len} exceeds server max {max_payload_bytes}"),
        ));
    }

    let mut buf = Vec::with_capacity(HEADER_SIZE + payload_len as usize);
    buf.extend_from_slice(&header_buf);
    if payload_len > 0 {
        buf.resize(HEADER_SIZE + payload_len as usize, 0);
        stream
            .read_exact(&mut buf[HEADER_SIZE..])
            .await
            .map_err(|e| {
                if e.kind() == io::ErrorKind::UnexpectedEof {
                    FrameReadError::Eof
                } else {
                    FrameReadError::Io(e)
                }
            })?;
    }

    let (frame, rest) = Frame::decode_with_max(&buf, max_payload_bytes).map_err(|e| {
        FrameReadError::Protocol(
            ErrorCode::BadFrame,
            ErrorCategory::Protocol,
            format!("frame decode: {e}"),
        )
    })?;
    debug_assert!(rest.is_empty(), "frame should consume the whole buffer");
    Ok(frame)
}

// ---------------------------------------------------------------------------
// Socket setup
// ---------------------------------------------------------------------------

fn bind_listener(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = if addr.is_ipv4() {
        TcpSocket::new_v4()?
    } else {
        TcpSocket::new_v6()?
    };
    // Spec §03/02 §1.2 — SO_REUSEADDR for graceful restart.
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    // Backlog 1024 is well above typical concurrent-accept rates and
    // below default kernel somaxconn (~4096 on stock Linux).
    socket.listen(1024)
}

fn configure_tcp(stream: &TcpStream) -> io::Result<()> {
    // Spec §03/02 §1.2: TCP_NODELAY + SO_KEEPALIVE.
    stream.set_nodelay(true)?;
    let sock = SockRef::from(stream);
    let keepalive = TcpKeepalive::new()
        .with_time(Duration::from_secs(75))
        .with_interval(Duration::from_secs(15))
        .with_retries(9);
    sock.set_tcp_keepalive(&keepalive)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec_caps() {
        let limits = ConnectionLimits::default();
        assert_eq!(limits.max_payload_bytes as usize, MAX_PAYLOAD_BYTES);
        assert_eq!(limits.read_timeout, Duration::from_secs(30));
    }
}
