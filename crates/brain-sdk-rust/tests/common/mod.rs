//! Shared mock-server fixture for the 10.5 op-method
//! integration tests.
//!
//! Each test:
//! 1. Calls [`spawn_mock_server`] to bring up a fresh listener.
//! 2. Hands `handler` a closure that runs the handshake then
//!    drives the op-specific request/response sequence.
//!
//! The mock honors `client_id` / agent_id on the wire but
//! doesn't validate against any state; it just decodes what the
//! client sends and writes back canned responses.

#![allow(dead_code)] // Each integration test only uses a subset.

use std::future::Future;
use std::net::SocketAddr;

use brain_protocol::handshake::{
    AgentPermissions, AuthMethod, AuthOkPayload, HelloCapabilities, ServerFeatures, WelcomePayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, RequestBody, ResponseBody};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const FLAG_EOS: u8 = 1 << 7;

/// Spawn a mock server. The `handler` runs once per accepted
/// connection, after the handshake is complete.
pub async fn spawn_mock_server<F, Fut>(handler: F) -> (SocketAddr, tokio::task::JoinHandle<()>)
where
    F: FnOnce(TcpStream) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send,
{
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let h = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        if !complete_handshake(&mut socket).await {
            return;
        }
        handler(socket).await;
    });
    (addr, h)
}

/// Run the HELLO/WELCOME/AUTH/AUTH_OK script. Returns true on
/// success, false on any decode failure.
pub async fn complete_handshake(socket: &mut TcpStream) -> bool {
    let Some(hello_frame) = read_frame_opt(socket).await else {
        return false;
    };
    if hello_frame.header.opcode_u16() != Opcode::Hello.as_u16() {
        return false;
    }

    let welcome = WelcomePayload {
        server_id: "mock-server".into(),
        chosen_version: brain_protocol::header::VERSION,
        session_id: [0xCD; 16],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        server_features: ServerFeatures {
            max_payload_size: brain_protocol::MAX_PAYLOAD_BYTES as u32,
            max_concurrent_streams: 1024,
            idle_timeout_seconds: 300,
            auth_methods: vec![AuthMethod::None],
        },
    };
    write_frame(
        socket,
        Opcode::Welcome.as_u16(),
        0,
        ResponseBody::Welcome(welcome).encode(),
        true,
    )
    .await;

    let Some(auth_frame) = read_frame_opt(socket).await else {
        return false;
    };
    if auth_frame.header.opcode_u16() != Opcode::Auth.as_u16() {
        return false;
    }
    let Ok(auth_body) = RequestBody::decode(Opcode::Auth, &auth_frame.payload) else {
        return false;
    };
    let agent_id = match auth_body {
        RequestBody::Auth(a) => a.agent_id,
        _ => return false,
    };

    let auth_ok = AuthOkPayload {
        agent_id,
        bound_shard_id: 0,
        permissions: AgentPermissions {
            can_encode: true,
            can_recall: true,
            can_plan: true,
            can_reason: true,
            can_forget: true,
            can_admin: false,
        },
        server_time_unix_nanos: 0,
    };
    write_frame(
        socket,
        Opcode::AuthOk.as_u16(),
        0,
        ResponseBody::AuthOk(auth_ok).encode(),
        true,
    )
    .await;
    true
}

/// Read one frame from the socket. Returns `None` on EOF /
/// decode error.
pub async fn read_frame_opt(socket: &mut TcpStream) -> Option<Frame> {
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    if socket.read_exact(&mut header).await.is_err() {
        return None;
    }
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        if socket
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .is_err()
        {
            return None;
        }
    }
    let (frame, rest) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).ok()?;
    debug_assert!(rest.is_empty());
    Some(frame)
}

pub async fn read_frame(socket: &mut TcpStream) -> Frame {
    read_frame_opt(socket).await.expect("frame expected")
}

/// Write a frame. `eos` controls the FLAG_EOS bit on the header.
pub async fn write_frame(
    socket: &mut TcpStream,
    opcode: u16,
    stream_id: u32,
    payload: Vec<u8>,
    eos: bool,
) {
    let flags = if eos { FLAG_EOS } else { 0 };
    let frame = Frame::new(opcode, flags, stream_id, payload);
    let _ = socket.write_all(&frame.encode()).await;
    let _ = socket.flush().await;
}
