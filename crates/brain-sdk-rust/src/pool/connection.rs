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

use brain_core::AgentId;
use brain_protocol::opcode::Opcode;
use brain_protocol::request::ByeRequest;
use brain_protocol::{Frame, RequestBody};
use tokio::net::TcpStream;

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

impl Connection {
    /// Open one connection: TCP connect → handshake → ready.
    pub async fn open(
        addr: SocketAddr,
        agent_id: AgentId,
        config: &ClientConfig,
    ) -> Result<Self, ClientError> {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(ClientError::Connect)?;
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
