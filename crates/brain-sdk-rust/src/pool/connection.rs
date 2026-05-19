//! `Connection` — one TCP connection to a brain-server, post-
//! handshake. Extracted from 10.1's `Client` so the pool can hold
//! many of them.
//!
//! `Connection` is not `Send + Sync` by accident — `TcpStream` is
//! `Send` so any callers (`Pool::acquire` returns `&mut Connection`
//! via a guard) get the right ergonomics. The pool's mutex serialises
//! external access.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use brain_core::AgentId;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{ByeRequest, ClientPongRequest};
use brain_protocol::{Frame, RequestBody, ResponseBody};
use socket2::{SockRef, TcpKeepalive};
use tokio::net::TcpStream;
use tokio::sync::oneshot;

use crate::config::ClientConfig;
use crate::error::ClientError;
use crate::proto::frames::{read_one_frame, write_frame};
use crate::proto::handshake::{complete_handshake, ClientIdentity, NegotiatedSession};

/// Spec §03/03 §4 — last-frame-of-stream flag.
const FLAG_EOS: u8 = 1 << 7;

/// One TCP connection that has completed the handshake.
///
/// Owns:
/// - The raw `TcpStream`.
/// - The per-connection `next_stream_id` allocator (spec §03/07 §3
///   — client streams are odd-numbered).
/// - The agent id the server bound to this connection in AUTH_OK.
/// - The negotiated session (WELCOME + AUTH_OK payloads).
#[derive(Debug)]
pub struct Connection {
    stream: TcpStream,
    session: NegotiatedSession,
    next_stream_id: AtomicU32,
    agent_id: AgentId,
}

/// TCP keepalive parameters (Step 1 of the liveness work).
///
/// The Linux defaults are useless for a database client (idle 7200 s
/// + interval 75 s × 9 retries ≈ 2.25 h before detection of a dead
/// peer). We pick aggressive but kind values:
///
/// - **idle 30 s**  — start probing 30 s after the last byte.
/// - **interval 10 s** — one probe per 10 s.
/// - **retries 3** — give up after 3 missed probes.
///
/// Net detection budget: ~60 s for a half-broken connection
/// (router gone, NAT timeout, peer host crashed silently). Cheap
/// vs the cost of an SDK op stalling for the kernel's default
/// 2-hour window.
///
/// The retries field isn't exposed by `socket2::TcpKeepalive` on
/// every platform (Linux ✓, macOS ✕, Windows ✕), so we set it via
/// the platform-conditional setter and leave the OS default
/// elsewhere — same idle + interval everywhere, retries only on
/// Linux. This still bounds detection on macOS-dev /
/// Windows-curious operators (~80 s with the OS default 8 retries).
const KEEPALIVE_IDLE: Duration = Duration::from_secs(30);
const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(10);
#[cfg(any(target_os = "linux", target_os = "freebsd"))]
const KEEPALIVE_RETRIES: u32 = 3;

/// Enable kernel-level TCP keepalive on the given stream. Idempotent.
/// Per-stream side-effect; the stream's behaviour is unchanged
/// otherwise (still owned by the caller).
fn set_keepalive(stream: &TcpStream) -> std::io::Result<()> {
    let sock_ref = SockRef::from(stream);
    sock_ref.set_keepalive(true)?;
    let mut ka = TcpKeepalive::new()
        .with_time(KEEPALIVE_IDLE)
        .with_interval(KEEPALIVE_INTERVAL);
    #[cfg(any(target_os = "linux", target_os = "freebsd"))]
    {
        ka = ka.with_retries(KEEPALIVE_RETRIES);
    }
    sock_ref.set_tcp_keepalive(&ka)?;
    Ok(())
}

impl Connection {
    /// Open one connection: TCP connect → keepalive → handshake → ready.
    pub async fn open(
        addr: SocketAddr,
        agent_id: AgentId,
        config: &ClientConfig,
    ) -> Result<Self, ClientError> {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(ClientError::Connect)?;
        // Kernel-level liveness backstop. Failure to set keepalive
        // (rare — only happens on exotic platforms / sandboxed
        // sockets) is logged but non-fatal; the SDK's app-level
        // CLIENT_PONG path is the primary mechanism.
        if let Err(e) = set_keepalive(&stream) {
            tracing::debug!(
                error = %e,
                "set_keepalive failed; SDK relies on CLIENT_PONG instead",
            );
        }
        let identity = ClientIdentity::v1("brain-sdk-rust");
        let session = complete_handshake(&mut stream, identity, agent_id, config.auth).await?;
        Ok(Self {
            stream,
            session,
            // Spec §03/07 §3: first client-initiated stream is 1.
            next_stream_id: AtomicU32::new(1),
            agent_id,
        })
    }

    /// The agent id stamped by the server in AUTH_OK.
    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// The negotiated session.
    #[must_use]
    pub fn session(&self) -> &NegotiatedSession {
        &self.session
    }

    /// Allocate the next client-initiated stream id. 10.5+ uses
    /// this from each op-method call.
    ///
    /// Spec §03/07 §3: client streams are odd. We increment by 2
    /// each call.
    #[allow(dead_code)] // Consumed by op methods in 10.5.
    pub(crate) fn next_stream_id(&self) -> u32 {
        self.next_stream_id.fetch_add(2, Ordering::Relaxed)
    }

    /// Mutable access to the underlying stream. Used by op
    /// methods (10.5+) to read / write frames; gated behind
    /// `pub(crate)` because direct frame I/O is an SDK-internal
    /// concern.
    #[allow(dead_code)] // Consumed by op methods in 10.5.
    pub(crate) fn stream_mut(&mut self) -> &mut TcpStream {
        &mut self.stream
    }

    /// Send a BYE on the control stream and consume the
    /// connection. Tolerates a clean `Closed` from the server (it
    /// may close right after our BYE without acking).
    pub async fn bye(mut self) -> Result<(), ClientError> {
        let bye = ByeRequest {
            reason: Some("brain-sdk-rust connection close".into()),
        };
        let frame = Frame::new(
            Opcode::Bye.as_u16(),
            FLAG_EOS,
            // Spec §03/08 §1: BYE travels on the control stream.
            0,
            RequestBody::Bye(bye).encode(),
        );
        write_frame(&mut self.stream, &frame).await?;
        match read_one_frame(&mut self.stream).await {
            Ok(resp) if resp.header.opcode_u16() == Opcode::Bye.as_u16() => Ok(()),
            Ok(other) => Err(ClientError::Protocol(
                brain_protocol::error::ProtocolError::BadFrame(format!(
                    "expected BYE echo, got opcode 0x{:02x}",
                    other.header.opcode_u16()
                )),
            )),
            Err(ClientError::Closed) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

// ---------------------------------------------------------------------------
// IdleConnection — actively-pong'd connection sitting in the pool's Idle slot.
// ---------------------------------------------------------------------------
//
// Spec §03/02 §6.1: the server emits SERVER_PING after `idle_timeout`
// (default 300 s) and closes the connection if CLIENT_PONG doesn't
// arrive within `ping_timeout` (default 30 s). A pool slot that sits
// truly idle (no app ops) has nobody reading frames, so SERVER_PING
// accumulates in the kernel buffer until the close fires.
//
// `IdleConnection` fixes this: when `Pool::release` puts a connection
// back into the Idle slot, it's wrapped in `IdleConnection` which
// spawns a tokio task that owns the stream, reads frames, and
// auto-responds to SERVER_PING. When `Pool::try_take_idle` pulls the
// slot for an op, `IdleConnection::into_active().await` cancels the
// background task, recovers the stream, and rebuilds a `Connection`
// for the caller.
//
// Why one tokio task per Idle connection (not a shared reader):
// - Each connection's read loop is independent. A shared reader would
//   need a map (slot_id → notify) and frame demultiplexing — more
//   complexity for no clarity win in v1.
// - The task is cheap (one stack-allocated future, one oneshot pair).
//
// Why ownership handoff (not split read/write halves):
// - Single owner per stream at any time. No coordination races, no
//   shared write-half mutex, no risk of interleaved partial writes.
// - The op's hot path (acquire → write request → read response) sees
//   exactly the same `&mut TcpStream` API it always did.
//
// Comparable production designs: this matches NATS's client-side
// idle-PING handler and gRPC's HTTP/2 keepalive ping responder.

/// One pool-slot's worth of connection state while sitting idle.
/// Hold while the background pong task runs; consume via
/// [`Self::into_active`] to take the stream back.
#[derive(Debug)]
pub struct IdleConnection {
    /// Sender for "give the stream back." The background task awaits
    /// on this in a `select!`; firing it (or dropping it) causes
    /// the task to send the stream onto `rejoin_rx` and exit.
    cancel_tx: oneshot::Sender<()>,
    /// Receiver for the recovered stream. The background task sends
    /// `Some(stream)` on normal cancel, or drops the sender (we
    /// observe `Err(RecvError)`) if the connection died mid-pong.
    rejoin_rx: oneshot::Receiver<TcpStream>,
    session: NegotiatedSession,
    next_stream_id: AtomicU32,
    agent_id: AgentId,
}

impl IdleConnection {
    /// Hand a recently-active [`Connection`] to a background task
    /// that pongs any incoming SERVER_PING.
    #[must_use]
    pub(crate) fn from_active(conn: Connection) -> Self {
        let Connection {
            stream,
            session,
            next_stream_id,
            agent_id,
        } = conn;
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (rejoin_tx, rejoin_rx) = oneshot::channel();
        tokio::spawn(idle_pong_loop(stream, cancel_rx, rejoin_tx));
        Self {
            cancel_tx,
            rejoin_rx,
            session,
            next_stream_id,
            agent_id,
        }
    }

    /// Cancel the background pong task, recover the stream, return an
    /// active [`Connection`]. Returns `Err(ClientError::Closed)` if
    /// the background task exited (Io error, server closed,
    /// protocol-fatal frame) before cancel reached it — the pool's
    /// caller should mark the slot Closed and retry on a fresh
    /// connection.
    pub(crate) async fn into_active(self) -> Result<Connection, ClientError> {
        // Send is best-effort: if the background task already exited
        // it dropped cancel_rx, send returns Err. Either way, the
        // truth is rejoin_rx — if the task sent a stream we get it;
        // otherwise we get RecvError and report Closed.
        let _ = self.cancel_tx.send(());
        match self.rejoin_rx.await {
            Ok(stream) => Ok(Connection {
                stream,
                session: self.session,
                next_stream_id: self.next_stream_id,
                agent_id: self.agent_id,
            }),
            Err(_recv_err) => Err(ClientError::Closed),
        }
    }
}

/// Body of the per-`IdleConnection` background task. Owns the stream
/// for the duration; pongs incoming SERVER_PING; exits cleanly on
/// `cancel_rx` signal by returning the stream via `rejoin_tx`. On
/// fatal read/write error, drops `rejoin_tx` without sending —
/// `into_active` will then observe `RecvError` and return `Closed`.
async fn idle_pong_loop(
    mut stream: TcpStream,
    mut cancel_rx: oneshot::Receiver<()>,
    rejoin_tx: oneshot::Sender<TcpStream>,
) {
    loop {
        tokio::select! {
            biased;
            // 1. Caller wants the stream back — return it and exit.
            _ = &mut cancel_rx => {
                let _ = rejoin_tx.send(stream);
                return;
            }
            // 2. A frame arrived from the server.
            result = read_one_frame(&mut stream) => {
                match result {
                    Ok(frame) => {
                        let opcode = frame.header.opcode_u16();
                        if opcode == Opcode::ServerPing.as_u16() {
                            // Echo the server's timestamp + our now.
                            // Per spec §03/02 §6.1 the contents matter
                            // less than the timely response; the server
                            // resets its idle timer on any inbound
                            // frame and the ping_timeout deadline
                            // specifically on receipt of CLIENT_PONG.
                            let server_ts = match ResponseBody::decode(
                                Opcode::ServerPing,
                                &frame.payload,
                            ) {
                                Ok(ResponseBody::ServerPing(p)) => {
                                    p.server_timestamp_unix_nanos
                                }
                                _ => 0,
                            };
                            let pong = ClientPongRequest {
                                server_timestamp_unix_nanos: server_ts,
                                client_timestamp_unix_nanos: now_unix_nanos(),
                            };
                            let pong_frame = Frame::new(
                                Opcode::ClientPong.as_u16(),
                                FLAG_EOS,
                                // Spec §03/08 §1: control-stream traffic.
                                0,
                                RequestBody::ClientPong(pong).encode(),
                            );
                            if write_frame(&mut stream, &pong_frame).await.is_err() {
                                // Write failed — connection dead.
                                // Drop rejoin_tx; caller sees Closed.
                                return;
                            }
                            // Loop and resume reading.
                        } else {
                            // Unexpected frame on a quiescent
                            // connection. v1 doesn't have spontaneous
                            // server-push frames outside of subscribe
                            // streams (which run on their own
                            // connection). Most likely cause: a late
                            // response from a cancelled op or a
                            // protocol misalignment. Log and continue
                            // — don't poison the slot for ambiguous
                            // input. If it becomes a real problem the
                            // slot can be hard-reset by the caller on
                            // the next acquire.
                            tracing::debug!(
                                opcode = format!("0x{:04x}", opcode),
                                "unexpected frame on idle SDK connection — ignoring",
                            );
                        }
                    }
                    Err(_) => {
                        // Read failed (Io / Closed / Protocol). The
                        // connection is unusable; drop rejoin_tx so
                        // the next take_idle sees Closed.
                        return;
                    }
                }
            }
        }
    }
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
