//! `Client` — the SDK's entry point.
//!
//! 10.1 ships a single-connection client. Subsequent sub-tasks
//! grow this: 10.2 introduces a pool (`pool::ServerConnections`),
//! 10.5 hangs op methods off `impl Client`, etc.
//!
//! Spec §13/02 §1-§2.

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
const FLAG_EOS: u16 = 1 << 15;

/// Single-connection client. Owns one `TcpStream`, the
/// negotiated handshake outcome, a per-connection stream-id
/// allocator (odd-numbered for client-initiated streams, spec
/// §03/07 §3), and the `ClientConfig` used at construction time.
#[derive(Debug)]
pub struct Client {
    stream: TcpStream,
    config: ClientConfig,
    session: NegotiatedSession,
    /// Next client-initiated stream id. Spec §03/07 §3: client
    /// streams are odd, server streams are even. Stream 0 is the
    /// control stream and is consumed by the handshake.
    next_stream_id: AtomicU32,
    /// The agent id bound to this connection (echoed from the
    /// server in AUTH_OK).
    agent_id: AgentId,
}

impl Client {
    /// Open a single TCP connection to `addr`, drive the
    /// handshake to completion using `config`'s auth method, and
    /// return a ready-to-use `Client`.
    pub async fn connect(addr: SocketAddr) -> Result<Self, ClientError> {
        Self::connect_with(addr, AgentId::new(), ClientConfig::default()).await
    }

    /// Like [`connect`] but with an explicit agent id and
    /// configuration. Most callers should use [`connect`] until
    /// they need to override defaults.
    ///
    /// [`connect`]: Client::connect
    pub async fn connect_with(
        addr: SocketAddr,
        agent_id: AgentId,
        config: ClientConfig,
    ) -> Result<Self, ClientError> {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(ClientError::Connect)?;
        let identity = ClientIdentity::v1("brain-sdk-rust");
        let session = complete_handshake(&mut stream, identity, agent_id, config.auth).await?;
        Ok(Self {
            stream,
            config,
            session,
            // Spec §03/07 §3: first client-initiated stream is 1.
            next_stream_id: AtomicU32::new(1),
            agent_id,
        })
    }

    /// The agent id the server bound to this connection.
    #[must_use]
    pub fn agent_id(&self) -> AgentId {
        self.agent_id
    }

    /// The negotiated session — WELCOME + AUTH_OK payloads. Mostly
    /// useful for inspecting `bound_shard_id` or the negotiated
    /// `max_payload_size` / `max_concurrent_streams`.
    #[must_use]
    pub fn session(&self) -> &NegotiatedSession {
        &self.session
    }

    /// The config the client was built with.
    #[must_use]
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Allocate the next client-initiated stream id. 10.5+ uses
    /// this from each op-method call.
    ///
    /// Spec §03/07 §3: client streams are odd. We increment by 2
    /// each call. Wrapping is fine — 2^31 client streams per
    /// connection is well beyond the spec's
    /// `max_concurrent_streams` (default 1024).
    #[allow(dead_code)] // Consumed by op methods in 10.5.
    pub(crate) fn next_stream_id(&self) -> u32 {
        self.next_stream_id.fetch_add(2, Ordering::Relaxed)
    }

    /// Send a `BYE` and consume the client, closing the
    /// connection cleanly. Spec §03/08 §1 — server echoes BYE
    /// and closes the socket.
    pub async fn bye(mut self) -> Result<(), ClientError> {
        let bye = ByeRequest {
            reason: Some("brain-sdk-rust client shutdown".into()),
        };
        let frame = Frame::new(
            Opcode::Bye.as_u8(),
            FLAG_EOS,
            // Spec §03/08 §1: BYE travels on the control stream.
            0,
            RequestBody::Bye(bye).encode(),
        );
        write_frame(&mut self.stream, &frame).await?;
        // Read the server's echoed BYE. Some servers may also
        // close without acking; tolerate both.
        match read_one_frame(&mut self.stream).await {
            Ok(resp) if resp.header.opcode == Opcode::Bye.as_u8() => Ok(()),
            Ok(other) => Err(ClientError::Protocol(
                brain_protocol::error::ProtocolError::BadFrame(format!(
                    "expected BYE echo, got opcode 0x{:02x}",
                    other.header.opcode
                )),
            )),
            Err(ClientError::Closed) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_stream_id_is_odd_and_monotonic() {
        let stream_id_counter = AtomicU32::new(1);
        let a = stream_id_counter.fetch_add(2, Ordering::Relaxed);
        let b = stream_id_counter.fetch_add(2, Ordering::Relaxed);
        let c = stream_id_counter.fetch_add(2, Ordering::Relaxed);
        assert_eq!((a, b, c), (1, 3, 5));
        assert!(a % 2 == 1 && b % 2 == 1 && c % 2 == 1);
    }
}
