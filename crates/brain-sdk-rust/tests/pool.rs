//! Integration tests for sub-task 10.2 — Pool semantics.
//!
//! Each test wires a fresh `tokio::net::TcpListener` mock server
//! that runs the canned HELLO → AUTH_OK script as many times as
//! the test needs. No cross-crate dep on brain-server.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use brain_core::AgentId;
use brain_protocol::handshake::{
    AgentPermissions, AuthMethod, AuthOkPayload, HelloCapabilities, ServerFeatures, WelcomePayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::{Frame, RequestBody, ResponseBody};
use brain_sdk_rust::{Client, ClientConfig, ClientError, Pool, PoolConfig};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// Mock server
// ---------------------------------------------------------------------------

/// Spawn a mock server that accepts an unbounded number of
/// connections, runs the handshake script on each, then idles
/// until EOF.
///
/// Returns `(addr, accept_counter, handle)`. `accept_counter`
/// increments after each successful HELLO+AUTH cycle so tests can
/// assert on connection counts.
async fn spawn_mock_server() -> (SocketAddr, Arc<AtomicUsize>, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let counter = Arc::new(AtomicUsize::new(0));
    let counter_clone = counter.clone();
    let handle = tokio::spawn(async move {
        loop {
            let (mut socket, _peer) = match listener.accept().await {
                Ok(pair) => pair,
                Err(_) => break,
            };
            let c = counter_clone.clone();
            tokio::spawn(async move {
                run_handshake_then_idle(&mut socket, &c).await;
            });
        }
    });
    (addr, counter, handle)
}

async fn run_handshake_then_idle(socket: &mut TcpStream, counter: &AtomicUsize) {
    // ---- HELLO ----------------------------------------------------
    let hello_frame = match read_frame_opt(socket).await {
        Some(f) => f,
        None => return,
    };
    if hello_frame.header.opcode_u16() != Opcode::Hello.as_u16() {
        return;
    }

    // ---- WELCOME --------------------------------------------------
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
    )
    .await;

    // ---- AUTH -----------------------------------------------------
    let auth_frame = match read_frame_opt(socket).await {
        Some(f) => f,
        None => return,
    };
    if auth_frame.header.opcode_u16() != Opcode::Auth.as_u16() {
        return;
    }
    let auth_body = match RequestBody::decode(Opcode::Auth, &auth_frame.payload) {
        Ok(b) => b,
        Err(_) => return,
    };
    let agent_id = match auth_body {
        RequestBody::Auth(a) => a.agent_id,
        _ => return,
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
    counter.fetch_add(1, Ordering::Relaxed);

    // ---- Echo BYE / drain until client disconnects ----------------
    while let Some(frame) = read_frame_opt(socket).await {
        if frame.header.opcode_u16() == Opcode::Bye.as_u16() {
            // Match brain-server's on_bye: same opcode in both
            // directions, payload echoed verbatim.
            write_frame(socket, Opcode::Bye.as_u16(), 0, frame.payload).await;
            return;
        }
        // Drop other frames silently in this mock.
    }
}

async fn read_frame_opt(socket: &mut TcpStream) -> Option<Frame> {
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

async fn write_frame(socket: &mut TcpStream, opcode: u16, stream_id: u32, payload: Vec<u8>) {
    let frame = Frame::new(opcode, FLAG_EOS, stream_id, payload);
    let _ = socket.write_all(&frame.encode()).await;
    let _ = socket.flush().await;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn warm_up_opens_min_connections() {
    let (addr, accepts, _server) = spawn_mock_server().await;

    let cfg = ClientConfig::default().with_pool(PoolConfig::new().with_max(8).with_min(3));
    let pool = Pool::new(addr, AgentId::new(), cfg);
    pool.warm_up().await.expect("warm_up");

    // Wait briefly for the mock handlers to record their counts.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        accepts.load(Ordering::Relaxed),
        3,
        "warm_up should open exactly min_connections (3)"
    );
    assert_eq!(pool.live_slots(), 3);
}

#[tokio::test]
async fn acquire_reuses_idle_connection() {
    let (addr, accepts, _server) = spawn_mock_server().await;

    let cfg = ClientConfig::default().with_pool(PoolConfig::new().with_max(4).with_min(1));
    let pool = Pool::new(addr, AgentId::new(), cfg);
    pool.warm_up().await.expect("warm_up");

    let g1 = pool.acquire().await.expect("acquire 1");
    drop(g1);
    let _g2 = pool.acquire().await.expect("acquire 2");

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        accepts.load(Ordering::Relaxed),
        1,
        "second acquire should reuse the warmed connection"
    );
}

#[tokio::test]
async fn acquire_blocks_then_succeeds_when_released() {
    let (addr, _accepts, _server) = spawn_mock_server().await;

    let cfg = ClientConfig::default().with_pool(PoolConfig::new().with_max(1).with_min(0));
    let pool = Pool::new(addr, AgentId::new(), cfg);

    let p1 = pool.clone();
    let p2 = pool.clone();

    let first = tokio::spawn(async move {
        let g = p1.acquire().await.expect("acquire 1");
        tokio::time::sleep(Duration::from_millis(100)).await;
        drop(g);
    });

    // Tiny wait to ensure `first` has the slot.
    tokio::time::sleep(Duration::from_millis(20)).await;
    let waiter = tokio::spawn(async move {
        let start = std::time::Instant::now();
        let _g = p2.acquire().await.expect("acquire 2");
        start.elapsed()
    });

    first.await.expect("first task");
    let waited = waiter.await.expect("waiter task");
    assert!(
        waited >= Duration::from_millis(50),
        "second acquire should have waited for the release ({waited:?})"
    );
}

#[tokio::test]
async fn acquire_overloaded_at_cap() {
    let (addr, _accepts, _server) = spawn_mock_server().await;

    let cfg = ClientConfig::default().with_pool(
        PoolConfig::new()
            .with_max(1)
            .with_min(0)
            .with_acquire_timeout(Duration::from_millis(80)),
    );
    let pool = Pool::new(addr, AgentId::new(), cfg);

    let _g = pool.acquire().await.expect("acquire 1");
    let start = std::time::Instant::now();
    let result = pool.acquire().await;
    let elapsed = start.elapsed();

    match result {
        Err(ClientError::Overloaded { .. }) => { /* expected */ }
        other => panic!("expected Overloaded, got {other:?}"),
    }
    assert!(
        elapsed >= Duration::from_millis(50),
        "acquire should respect acquire_timeout ({elapsed:?})"
    );
    assert!(
        elapsed < Duration::from_millis(500),
        "acquire should return well before the request timeout ({elapsed:?})"
    );
}

#[tokio::test]
async fn idle_reaper_closes_stale_connections() {
    let (addr, _accepts, _server) = spawn_mock_server().await;

    let cfg = ClientConfig::default().with_pool(
        PoolConfig::new()
            .with_max(4)
            .with_min(0)
            .with_idle_timeout(Duration::from_millis(100)),
    );
    let pool = Pool::new(addr, AgentId::new(), cfg);

    {
        let _g = pool.acquire().await.expect("acquire");
    }
    assert_eq!(pool.live_slots(), 1);

    // Reaper wakes every idle_timeout / 4 = 25ms. After 400ms the
    // slot should have been reaped (idle past 100ms).
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        pool.live_slots(),
        0,
        "reaper should close the idle slot once past idle_timeout"
    );
}

#[tokio::test]
async fn close_rejects_further_acquires() {
    let (addr, _accepts, _server) = spawn_mock_server().await;

    let cfg = ClientConfig::default().with_pool(PoolConfig::single());
    let pool = Pool::new(addr, AgentId::new(), cfg);
    pool.warm_up().await.expect("warm_up");
    pool.close();

    let result = pool.acquire().await;
    assert!(matches!(result, Err(ClientError::PoolClosed)));
}

#[tokio::test]
async fn client_connect_still_works() {
    // 10.1 compatibility: Client::connect must still open one
    // connection and complete the handshake.
    let (addr, accepts, _server) = spawn_mock_server().await;

    let client = Client::connect(addr).await.expect("connect");
    assert_eq!(client.config().pool.max_connections, 1);

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(accepts.load(Ordering::Relaxed), 1);

    client.bye().await.expect("bye");
}
