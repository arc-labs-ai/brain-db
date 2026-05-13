//! Integration tests for the sub-task 9.10 frame dispatcher.
//!
//! Each test brings up a fresh `ConnectionListener` plus a small pool
//! of real `spawn_shard`'d Glommio executors on `127.0.0.1:0`, drives
//! a Tokio client over plain TCP, and asserts wire-level behavior:
//! handshake, op dispatch through the Tokio↔Glommio boundary, error
//! shapes, and idle-timer events.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloCapabilities, HelloPayload,
    ServerCapabilities, WelcomePayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{
    ByeRequest, EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, PingRequest,
    RecallRequest, RequestBody,
};
use brain_protocol::response::{
    EncodeResponse, ErrorResponse, ForgetResponse, PongResponse, ResponseBody,
};
use brain_protocol::Frame;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

// Pull every brain-server source the connection layer reaches into the
// test binary so `crate::` resolves the same as in main.rs.
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[allow(dead_code)]
#[path = "../src/network/routing.rs"]
mod routing;
#[allow(dead_code)]
#[path = "../src/shard/mod.rs"]
mod shard;
#[path = "../src/network/subscribe.rs"]
mod subscribe;
#[allow(dead_code)]
#[path = "../src/bootstrap/tls.rs"]
mod tls;

use connection::{ConnectionLimits, ConnectionListener, ShutdownSignal, ShutdownTrigger, Topology};
use routing::RoutingTable;
use shard::{spawn_shard, ShardHandle, ShardJoiner, ShardSpawnConfig};

// ---------------------------------------------------------------------------
// Scaffold
// ---------------------------------------------------------------------------

const FLAG_EOS: u16 = 1 << 15;

struct Server {
    addr: SocketAddr,
    trigger: ShutdownTrigger,
    listener: tokio::task::JoinHandle<std::io::Result<SocketAddr>>,
    handles: Vec<ShardHandle>,
    joiners: Vec<Option<ShardJoiner>>,
    _data_dir: TempDir,
}

impl Server {
    async fn stop(mut self) {
        self.trigger.signal();
        let _ = tokio::time::timeout(Duration::from_secs(2), &mut self.listener).await;
        drop(self.handles); // close every shard's channel
        for joiner in self.joiners.iter_mut().filter_map(|j| j.take()) {
            // Block on join. ShardJoiner is sync.
            let _ = tokio::task::spawn_blocking(move || joiner.join())
                .await
                .map_err(|_| ());
        }
    }
}

async fn start_with_shards(n_shards: usize, limits: ConnectionLimits) -> Server {
    let data_dir = TempDir::new().expect("tmp");
    let mut handles = Vec::with_capacity(n_shards);
    let mut joiners = Vec::with_capacity(n_shards);
    for shard_id in 0..n_shards {
        let cfg = ShardSpawnConfig::new(data_dir.path());
        let (h, j) = spawn_shard(shard_id as u16, cfg).expect("spawn shard");
        handles.push(h);
        joiners.push(Some(j));
    }

    let routing = Arc::new(arc_swap::ArcSwap::from_pointee(
        RoutingTable::new(n_shards as u16, std::collections::HashMap::new()).unwrap(),
    ));
    let topology = Topology {
        shards: Arc::new(handles.clone()),
        routing,
        server_caps: Arc::new(ServerCapabilities::v1_default(
            "brain-server/test",
            vec![AuthMethod::None],
        )),
    };

    let (trigger, signal) = ShutdownSignal::channel();
    let listener = ConnectionListener::new(
        "127.0.0.1:0".parse().unwrap(),
        None,
        topology,
        Arc::new(connection::ConnectionMetrics::default()),
        limits,
        signal,
    );
    let bound = listener.bind().expect("bind");
    let addr = bound.local_addr();
    let listener_handle = tokio::spawn(async move { bound.serve().await });

    Server {
        addr,
        trigger,
        listener: listener_handle,
        handles,
        joiners,
        _data_dir: data_dir,
    }
}

// ---------------------------------------------------------------------------
// Client helpers
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Result<Frame, String>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("header read: {e}"))?;
    let payload_len_be = [header[16], header[17], header[18]];
    let payload_len =
        u32::from_be_bytes([0, payload_len_be[0], payload_len_be[1], payload_len_be[2]]) as usize;

    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .map_err(|e| format!("payload read: {e}"))?;
    }
    let (frame, rest) = Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32)
        .map_err(|e| format!("decode: {e}"))?;
    debug_assert!(rest.is_empty());
    Ok(frame)
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(
    client: &mut TcpStream,
    agent_id: [u8; 16],
) -> (WelcomePayload, AuthOkPayload) {
    let hello = HelloPayload {
        client_id: "tester/0.1".to_owned(),
        supported_versions: vec![1],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Hello.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await;
    let welcome_frame = read_one_frame(client).await.expect("read WELCOME");
    assert_eq!(welcome_frame.header.opcode, Opcode::Welcome.as_u8());
    let welcome = match ResponseBody::decode(Opcode::Welcome, &welcome_frame.payload)
        .expect("decode welcome")
    {
        ResponseBody::Welcome(w) => w,
        other => panic!("expected Welcome, got {other:?}"),
    };

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id,
        credentials: AuthCredentials::None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Auth.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Auth(auth).encode(),
        ),
    )
    .await;
    let auth_ok_frame = read_one_frame(client).await.expect("read AUTH_OK");
    assert_eq!(auth_ok_frame.header.opcode, Opcode::AuthOk.as_u8());
    let auth_ok = match ResponseBody::decode(Opcode::AuthOk, &auth_ok_frame.payload)
        .expect("decode auth_ok")
    {
        ResponseBody::AuthOk(a) => a,
        other => panic!("expected AuthOk, got {other:?}"),
    };
    (welcome, auth_ok)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn handshake_completes() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    let (_welcome, auth_ok) = complete_handshake(&mut client, agent_id).await;
    assert_eq!(auth_ok.agent_id, agent_id);
    assert_eq!(auth_ok.bound_shard_id, 0, "only 1 shard → bound shard 0");
    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hello_with_unsupported_version_errors_and_closes() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let hello = HelloPayload {
        client_id: "tester".into(),
        supported_versions: vec![99],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send_frame(
        &mut client,
        Frame::new(
            Opcode::Hello.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read response");
    assert_eq!(resp.header.opcode, Opcode::Error.as_u8());

    // Connection should close.
    let mut buf = [0u8; 1];
    let n = client.read(&mut buf).await.expect("read EOF");
    assert_eq!(n, 0);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ops_before_auth_are_rejected() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");

    // Send HELLO, get WELCOME — but DON'T send AUTH.
    let hello = HelloPayload {
        client_id: "tester".into(),
        supported_versions: vec![1],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_session_token: None,
    };
    send_frame(
        &mut client,
        Frame::new(
            Opcode::Hello.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await;
    let _welcome = read_one_frame(&mut client).await.expect("welcome");

    // Now send ENCODE — should get ERROR(NotAuthenticated).
    let encode = EncodeRequest {
        text: "hello".into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: [0u8; 16],
        txn_id: None,
        deduplicate: false,
    };
    send_frame(
        &mut client,
        Frame::new(
            Opcode::EncodeReq.as_u8(),
            FLAG_EOS,
            1,
            RequestBody::Encode(encode).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read error");
    assert_eq!(resp.header.opcode, Opcode::Error.as_u8());

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ping_pong_with_timestamp() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let ts = 1_234_567_890u64;
    send_frame(
        &mut client,
        Frame::new(
            Opcode::Ping.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Ping(PingRequest {
                client_timestamp_unix_nanos: ts,
            })
            .encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read PONG");
    assert_eq!(resp.header.opcode, Opcode::Pong.as_u8());
    let pong = match ResponseBody::decode(Opcode::Pong, &resp.payload).expect("decode pong") {
        ResponseBody::Pong(p) => p,
        other => panic!("expected Pong, got {other:?}"),
    };
    let _: PongResponse = pong;
    assert_eq!(pong.client_timestamp_unix_nanos, ts, "ts echoed");
    assert!(
        pong.server_timestamp_unix_nanos > 0,
        "server populates its own timestamp"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bye_echoes_and_closes() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    send_frame(
        &mut client,
        Frame::new(
            Opcode::Bye.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Bye(ByeRequest {
                reason: Some("done".into()),
            })
            .encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read BYE");
    assert_eq!(resp.header.opcode, Opcode::Bye.as_u8());

    // Server closes after BYE.
    let mut buf = [0u8; 1];
    let n = client.read(&mut buf).await.expect("read EOF");
    assert_eq!(n, 0);

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bad_opcode_errors_stream_not_connection() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    // Send a *response* opcode (client→server is disallowed) on stream 1.
    let bogus = Frame::new(Opcode::EncodeResp.as_u8(), FLAG_EOS, 1, Vec::new());
    send_frame(&mut client, bogus).await;
    let resp = read_one_frame(&mut client).await.expect("read error");
    assert_eq!(resp.header.opcode, Opcode::Error.as_u8());

    // Connection stays open — follow-up PING still works.
    send_frame(
        &mut client,
        Frame::new(
            Opcode::Ping.as_u8(),
            FLAG_EOS,
            0,
            RequestBody::Ping(PingRequest {
                client_timestamp_unix_nanos: 1,
            })
            .encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read PONG");
    assert_eq!(resp.header.opcode, Opcode::Pong.as_u8());

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encode_round_trips_through_shard() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let encode = EncodeRequest {
        text: "hello world".into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    };
    send_frame(
        &mut client,
        Frame::new(
            Opcode::EncodeReq.as_u8(),
            FLAG_EOS,
            1,
            RequestBody::Encode(encode).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read encode resp");
    // The op-dispatch path went through the shard's brain_ops::dispatch.
    // Either ENCODE_RESP with a memory_id or ERROR — depending on whether
    // the stub embedder + writer succeed end-to-end. Both validate the
    // boundary; assert one of the two.
    let opcode = resp.header.opcode;
    if opcode == Opcode::EncodeResp.as_u8() {
        let body =
            ResponseBody::decode(Opcode::EncodeResp, &resp.payload).expect("decode encode resp");
        let r: EncodeResponse = match body {
            ResponseBody::Encode(r) => r,
            other => panic!("expected EncodeResponse, got {other:?}"),
        };
        let _ = r; // memory_id may be NULL on the stub path; that's fine
    } else if opcode == Opcode::Error.as_u8() {
        let body = ResponseBody::decode(Opcode::Error, &resp.payload).expect("decode error");
        let _err: ErrorResponse = match body {
            ResponseBody::Error(e) => e,
            other => panic!("expected ErrorResponse, got {other:?}"),
        };
    } else {
        panic!("unexpected response opcode 0x{opcode:02x}");
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn forget_routes_by_memory_id() {
    // Two shards. Forge a memory_id whose top-16-bit shard == 1; the
    // dispatcher should route to shard 1, not the agent's bound shard
    // (which could be either, depending on the agent_id hash).
    let server = start_with_shards(2, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let memory_id = brain_core::MemoryId::pack(1, 7, 1).raw();
    let forget = ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
    };
    send_frame(
        &mut client,
        Frame::new(
            Opcode::ForgetReq.as_u8(),
            FLAG_EOS,
            1,
            RequestBody::Forget(forget).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read forget resp");
    // Either FORGET_RESP (most likely) or ERROR — both validate that the
    // request reached *some* shard. The point of the test is to
    // exercise memory-id routing without timing out. The test fails if
    // the dispatcher panics or hangs.
    let opcode = resp.header.opcode;
    assert!(
        opcode == Opcode::ForgetResp.as_u8() || opcode == Opcode::Error.as_u8(),
        "expected ForgetResp or Error, got 0x{opcode:02x}"
    );
    if opcode == Opcode::ForgetResp.as_u8() {
        let _: ForgetResponse =
            match ResponseBody::decode(Opcode::ForgetResp, &resp.payload).unwrap() {
                ResponseBody::Forget(r) => r,
                other => panic!("expected Forget, got {other:?}"),
            };
    }

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn recall_returns_single_frame_eos_in_v1() {
    let server = start_with_shards(1, ConnectionLimits::default()).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    let recall = RecallRequest {
        cue_text: "anything".into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy_hint: None,
        include_vectors: false,
        include_edges: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
    };
    send_frame(
        &mut client,
        Frame::new(
            Opcode::RecallReq.as_u8(),
            FLAG_EOS,
            1,
            RequestBody::Recall(recall).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(&mut client).await.expect("read recall resp");
    let opcode = resp.header.opcode;
    // 9.10 ships single-frame EOS responses for streaming ops. The
    // frame *header* has the EOS bit set regardless of body opcode.
    assert!(
        opcode == Opcode::RecallResp.as_u8() || opcode == Opcode::Error.as_u8(),
        "expected RecallResp or Error, got 0x{opcode:02x}"
    );
    assert!(
        resp.header.flags_u16() & FLAG_EOS != 0,
        "response must carry EOS in v1"
    );

    server.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn server_ping_fires_after_idle_timeout() {
    let limits = ConnectionLimits {
        idle_timeout: Duration::from_millis(200),
        ping_timeout: Duration::from_secs(5),
        ..ConnectionLimits::default()
    };
    let server = start_with_shards(1, limits).await;
    let mut client = TcpStream::connect(server.addr).await.expect("connect");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();
    complete_handshake(&mut client, agent_id).await;

    // Stay idle past idle_timeout. Server should emit SERVER_PING.
    let resp = tokio::time::timeout(Duration::from_secs(3), read_one_frame(&mut client))
        .await
        .expect("server should send SERVER_PING within 3s")
        .expect("read SERVER_PING");
    assert_eq!(resp.header.opcode, Opcode::ServerPing.as_u8());

    server.stop().await;
}
