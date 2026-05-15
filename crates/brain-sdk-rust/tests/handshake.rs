//! Integration test for sub-task 10.1: `Client::connect` against a
//! hand-rolled mock server that speaks the spec §03/06 handshake.
//!
//! Why a mock server instead of reaching into brain-server's
//! in-process scaffold: the SDK is a pure-client crate and should
//! NOT depend on brain-server (would create a server↔client cycle
//! at the dep-graph level). A mock server bound to `127.0.0.1:0`
//! is ~80 LOC and exercises the wire-level contract exactly the
//! same way a real server would.
//!
//! 10.13's e2e suite will drive a real brain-server subprocess
//! from the SDK; that test fixture covers the cross-crate
//! integration.

use std::net::SocketAddr;

use brain_core::AgentId;
use brain_protocol::handshake::{
    AgentPermissions, AuthMethod, AuthOkPayload, HelloCapabilities, ServerFeatures, WelcomePayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, RequestBody, ResponseBody};
use brain_sdk_rust::ClientError;
use brain_sdk_rust::{Client, ClientConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FLAG_EOS: u8 = 1 << 7;

/// Bind a mock server to a random port. Returns the bound address
/// and a `JoinHandle` running the canned HELLO→AUTH_OK→BYE script.
async fn spawn_mock_server() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let handle = tokio::spawn(async move {
        let (mut socket, _peer) = listener.accept().await.expect("accept");
        run_canned_script(&mut socket).await;
    });
    (addr, handle)
}

/// The mock server's script: read HELLO, write WELCOME, read AUTH,
/// write AUTH_OK, read BYE, write BYE, close.
async fn run_canned_script(socket: &mut TcpStream) {
    // ---- HELLO ----------------------------------------------------
    let hello_frame = read_frame(socket).await;
    assert_eq!(
        hello_frame.header.opcode_u16(),
        Opcode::Hello.as_u16(),
        "expected HELLO"
    );

    // ---- WELCOME --------------------------------------------------
    let welcome = WelcomePayload {
        server_id: "mock-server".into(),
        chosen_version: brain_protocol::header::VERSION,
        session_id: [0xAB; 16],
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
    )
    .await;

    // ---- AUTH -----------------------------------------------------
    let auth_frame = read_frame(socket).await;
    assert_eq!(
        auth_frame.header.opcode_u16(),
        Opcode::Auth.as_u16(),
        "expected AUTH"
    );
    let auth_body = RequestBody::decode(Opcode::Auth, &auth_frame.payload).expect("decode AUTH");
    let agent_id = match auth_body {
        RequestBody::Auth(a) => a.agent_id,
        _ => panic!("AUTH opcode but body variant didn't match"),
    };

    // ---- AUTH_OK --------------------------------------------------
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
    )
    .await;

    // ---- BYE (client-initiated; echo back) ------------------------
    // The connect-only test doesn't get this far, so we tolerate
    // either a BYE or EOF here. Spec §03/05 §1.1 — BYE is the same
    // opcode in both directions; the server's reply just echoes the
    // request payload (matches brain-server's `on_bye`).
    if let Some(frame) = read_frame_opt(socket).await {
        assert_eq!(
            frame.header.opcode_u16(),
            Opcode::Bye.as_u16(),
            "post-AUTH expected BYE, got 0x{:02x}",
            frame.header.opcode_u16()
        );
        write_frame(socket, Opcode::Bye.as_u16(), 0, frame.payload).await;
    }
}

async fn read_frame(socket: &mut TcpStream) -> Frame {
    read_frame_opt(socket).await.expect("frame expected")
}

async fn read_frame_opt(socket: &mut TcpStream) -> Option<Frame> {
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    match socket.read_exact(&mut header).await {
        Ok(_) => {}
        Err(_) => return None,
    }
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        socket
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .expect("payload read");
    }
    let (frame, rest) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    debug_assert!(rest.is_empty());
    Some(frame)
}

async fn write_frame(socket: &mut TcpStream, opcode: u16, stream_id: u32, payload: Vec<u8>) {
    let frame = Frame::new(opcode, FLAG_EOS, stream_id, payload);
    socket.write_all(&frame.encode()).await.expect("write");
    socket.flush().await.expect("flush");
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn connects_completes_handshake() {
    let (addr, server) = spawn_mock_server().await;

    let agent = AgentId::new();
    let client = Client::connect_with(addr, agent, ClientConfig::default())
        .await
        .expect("connect should succeed");

    let session = client
        .session()
        .await
        .expect("session")
        .expect("non-empty session");
    assert_eq!(
        session.welcome.chosen_version,
        brain_protocol::header::VERSION
    );
    assert_eq!(session.welcome.session_id, [0xAB; 16]);
    assert_eq!(session.welcome.server_features.max_concurrent_streams, 1024);
    assert_eq!(
        session.auth_ok.agent_id,
        *agent.0.as_bytes(),
        "AUTH_OK should echo our agent_id"
    );
    assert_eq!(session.auth_ok.bound_shard_id, 0);
    assert_eq!(client.agent_id(), agent);

    // BYE — server echoes and closes.
    client.bye().await.expect("bye");
    server.await.expect("server task");
}

#[tokio::test]
async fn connect_to_closed_port_returns_connect_error() {
    // Bind + drop to get a closed-but-known port.
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    drop(listener);

    let result = Client::connect(addr).await;
    match result {
        Ok(_) => panic!("connect to closed port should fail"),
        Err(ClientError::Connect(_)) => { /* expected */ }
        Err(other) => panic!("expected ClientError::Connect, got {other}"),
    }
}
