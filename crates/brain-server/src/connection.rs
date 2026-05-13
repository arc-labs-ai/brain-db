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
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::error::{ErrorCategory, ErrorCode};
use brain_protocol::opcode::Opcode;
use brain_protocol::response::{ErrorCategoryWire, ErrorCodeWire, ErrorResponse, ResponseBody};
use brain_protocol::{Frame, HEADER_SIZE, MAX_PAYLOAD_BYTES};
use socket2::{SockRef, TcpKeepalive};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpSocket, TcpStream};
use tokio::sync::watch;
use tokio_rustls::rustls::ServerConfig;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::shard::ShardHandle;

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
}

impl Default for ConnectionLimits {
    fn default() -> Self {
        Self {
            max_payload_bytes: MAX_PAYLOAD_BYTES as u32,
            read_timeout: Duration::from_secs(30),
        }
    }
}

/// Unbound listener config. Call [`ConnectionListener::bind`] to
/// produce a [`BoundConnectionListener`] (which exposes
/// [`BoundConnectionListener::local_addr`] before [`Self::serve`]).
pub struct ConnectionListener {
    listen_addr: SocketAddr,
    tls: Option<Arc<ServerConfig>>,
    shards: Arc<Vec<ShardHandle>>,
    limits: ConnectionLimits,
    shutdown: ShutdownSignal,
}

/// Listener that has already opened its TCP socket. The address is
/// observable via [`Self::local_addr`] before [`Self::serve`] is awaited.
pub struct BoundConnectionListener {
    listener: TcpListener,
    local_addr: SocketAddr,
    tls: Option<Arc<ServerConfig>>,
    #[allow(dead_code)] // wired into the dispatcher in 9.10
    shards: Arc<Vec<ShardHandle>>,
    limits: ConnectionLimits,
    shutdown: ShutdownSignal,
}

impl ConnectionListener {
    pub fn new(
        listen_addr: SocketAddr,
        tls: Option<Arc<ServerConfig>>,
        shards: Arc<Vec<ShardHandle>>,
        limits: ConnectionLimits,
        shutdown: ShutdownSignal,
    ) -> Self {
        Self {
            listen_addr,
            tls,
            shards,
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
            shards: self.shards,
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
                    tokio::spawn(async move {
                        let result = match acceptor {
                            Some(acceptor) => match acceptor.accept(stream).await {
                                Ok(tls_stream) => {
                                    serve_connection(tls_stream, limits, shutdown).await
                                }
                                Err(e) => {
                                    debug!(peer = %peer, error = %e, "TLS handshake failed");
                                    return;
                                }
                            },
                            None => serve_connection(stream, limits, shutdown).await,
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

/// One connection's lifetime. Stub body for 9.9: any well-formed frame
/// is answered with `ERROR(BadFrame, "9.10 not yet wired")` and the
/// connection is closed. Decode failures emit a matching ERROR frame
/// and close. 9.10 replaces the inner match with the real dispatcher.
pub(crate) async fn serve_connection<S>(
    mut stream: S,
    limits: ConnectionLimits,
    mut shutdown: ShutdownSignal,
) -> io::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // 9.9: the stub closes after exactly one frame, so this `loop {}`
    // never iterates. 9.10's real dispatcher keeps the loop and stops
    // returning early on the success arm, at which point the iteration
    // is meaningful. Keeping the structure now avoids a churn-only diff
    // in 9.10.
    #[allow(clippy::never_loop)]
    loop {
        tokio::select! {
            biased;
            () = shutdown.recv() => {
                return Ok(());
            }
            result = read_one_frame(&mut stream, limits.max_payload_bytes, limits.read_timeout) => {
                match result {
                    Ok(_frame) => {
                        // 9.9 stub: accept the frame as wire-valid,
                        // refuse to do anything with it, close.
                        write_error_frame(
                            &mut stream,
                            ErrorCode::BadFrame,
                            ErrorCategory::Protocol,
                            "frame dispatcher not wired (sub-task 9.10)",
                        )
                        .await?;
                        return Ok(());
                    }
                    Err(FrameReadError::Eof) => return Ok(()),
                    Err(FrameReadError::Protocol(code, category, detail)) => {
                        let _ = write_error_frame(&mut stream, code, category, &detail).await;
                        return Ok(());
                    }
                    Err(FrameReadError::Timeout) => {
                        // Per-frame deadline expired. Treat like EOF —
                        // close quietly; 9.10's idle PING is the proper
                        // application-level keepalive.
                        return Ok(());
                    }
                    Err(FrameReadError::Io(e)) => return Err(e),
                }
            }
        }
    }
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

async fn write_error_frame<S>(
    stream: &mut S,
    code: ErrorCode,
    category: ErrorCategory,
    message: &str,
) -> io::Result<()>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let body = ResponseBody::Error(ErrorResponse {
        code: ErrorCodeWire::from(code),
        category: ErrorCategoryWire::from(category),
        message: message.to_owned(),
        details: None,
        retry_after_ms: None,
    });
    let payload = body.encode();
    // stream_id = 0 — connection-level error (spec §03/03 §3.5).
    let frame = Frame::new(Opcode::Error.as_u8(), 0, 0, payload);
    let bytes = frame.encode();
    stream.write_all(&bytes).await?;
    stream.flush().await?;
    Ok(())
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
