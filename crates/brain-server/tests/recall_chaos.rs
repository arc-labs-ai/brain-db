//! Chaos: kill the server while a hybrid recall is in flight.
//!
//! **CH1** — issue a hybrid recall on a populated shard; concurrently
//! drop the shard handles + signal shutdown before the response is
//! fully read. The client must observe a clean failure (connection
//! reset / EOF / decode error against a truncated frame) within a
//! 2s timeout envelope. The test fails if:
//!
//! - The client hangs past 2s (no progress on shutdown).
//! - The server panics during teardown (caught by `JoinHandle::await`
//!   returning a panic).
//! - The shard joiner panics on join.
//!
//! Brain's WAL-before-acknowledge invariant means RECALL has no
//! durable effect either way; what matters here is shutdown
//! cleanliness, not transactional semantics.

#![cfg(target_os = "linux")]

use std::time::Duration;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, MemoryKindWire, RecallRequest};
use brain_protocol::Frame;
use brain_protocol::RequestBody;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
#[allow(dead_code)]
#[path = "../src/network/auth.rs"]
mod auth;
#[allow(dead_code)]
#[path = "../src/config/mod.rs"]
mod config;
#[allow(dead_code)]
#[path = "../src/network/connection.rs"]
mod connection;
#[path = "../src/network/dispatch.rs"]
mod dispatch;
#[path = "../src/metrics/mod.rs"]
mod metrics;
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

mod support_harness;

use support_harness::start;

const FLAG_EOS: u8 = 1 << 7;

async fn read_one_frame<S>(stream: &mut S) -> std::io::Result<Frame>
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream.read_exact(&mut header).await?;
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await?;
    }
    let (frame, _) = Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, format!("{e:?}")))?;
    Ok(frame)
}

async fn send_frame(client: &mut TcpStream, frame: Frame) -> std::io::Result<()> {
    client.write_all(&frame.encode()).await?;
    client.flush().await
}

async fn complete_handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "recall-chaos".into(),
        supported_versions: vec![brain_protocol::VERSION],
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
            Opcode::Hello.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Hello(hello).encode(),
        ),
    )
    .await
    .expect("send hello");
    let welcome = read_one_frame(client).await.expect("welcome");
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id: *uuid::Uuid::now_v7().as_bytes(),
        credentials: AuthCredentials::None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::Auth.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Auth(auth).encode(),
        ),
    )
    .await
    .expect("send auth");
    let auth_ok = read_one_frame(client).await.expect("auth_ok");
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

async fn encode_text(client: &mut TcpStream, stream_id: u32, text: &str) {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    };
    let body = RequestBody::Encode(req);
    let opcode = body.opcode().as_u16();
    let payload = body.encode();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, payload))
        .await
        .expect("send encode");
    let resp = read_one_frame(client).await.expect("encode resp");
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::EncodeResp.as_u16(),
        "encode failed: opcode={}",
        resp.header.opcode_u16(),
    );
}

async fn seed_fixture(client: &mut TcpStream) {
    let phrases = [
        "Priya prefers async meetings over standups",
        "Async-first communication reduces context-switching",
        "Standups are a sync ritual we should retire",
        "Document driven design helps async teams",
        "Team prefers structured documents over live calls",
    ];
    for (i, p) in phrases.iter().enumerate() {
        encode_text(client, 101 + (i as u32) * 2, p).await;
    }
}

fn recall_request() -> RecallRequest {
    RecallRequest {
        cue_text: "meeting preferences".into(),
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    }
}

// ---------------------------------------------------------------------------
// CH1 — kill during hybrid recall.
//
// Sequence:
//   1. start server, handshake, seed a non-trivial fixture.
//   2. Send the RECALL frame.
//   3. As soon as the bytes are flushed, signal shutdown (drop the
//      handles + listener via `Server::stop`).
//   4. Wait for the response read, wrapped in a 2s envelope. Either:
//      (a) we receive the recall response (server completed before
//          shutdown), and the test ends cleanly, or
//      (b) we observe EOF / connection reset / partial-frame decode
//          error — the connection layer cut us off mid-flight.
//   5. Both outcomes are valid. A hang past 2s is not.
//
// The test's whole-test timeout (separate from the read envelope)
// is also enforced so a stuck `Server::stop` surfaces here rather
// than as a generic test-runner timeout.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_mid_hybrid_recall_completes_within_two_seconds() {
    let outer = tokio::time::timeout(Duration::from_secs(15), async {
        let server = start(1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect");
        complete_handshake(&mut client).await;
        seed_fixture(&mut client).await;

        // Send the hybrid recall but do NOT read the response yet —
        // we want the server's reply (or its truncation) to race
        // with our shutdown signal.
        let body = RequestBody::Recall(recall_request());
        let opcode = body.opcode().as_u16();
        let payload = body.encode();
        send_frame(&mut client, Frame::new(opcode, FLAG_EOS, 1, payload))
            .await
            .expect("send recall");

        // Race the read against shutdown. We spawn the shutdown so
        // it lands on its own task and can preempt the read mid-frame.
        let stop_task = tokio::spawn(async move {
            // Brief jitter so the server has a chance to begin processing.
            // Not load-bearing; the test passes whether or not the
            // race ends before the response is fully written.
            tokio::time::sleep(Duration::from_millis(5)).await;
            server.stop().await;
        });

        let read_outcome =
            tokio::time::timeout(Duration::from_secs(2), read_one_frame(&mut client)).await;

        // 2s envelope: hung read is a hard failure.
        let inner = match read_outcome {
            Ok(inner) => inner,
            Err(_) => {
                panic!("client read hung > 2s waiting for recall response or EOF after shutdown");
            }
        };

        match inner {
            Ok(frame) => {
                // Server beat the shutdown — RECALL completed.
                // Verify the response is well-formed; a recall reply
                // OR a structured error reply both count as clean.
                let op = frame.header.opcode_u16();
                assert!(
                    op == Opcode::RecallResp.as_u16() || op == Opcode::Error.as_u16(),
                    "unexpected reply opcode {op} during shutdown race",
                );
            }
            Err(e) => {
                // Acceptable: EOF (UnexpectedEof), reset (ConnectionReset / BrokenPipe),
                // or partial-frame decode (InvalidData from `decode_with_max`).
                let kind = e.kind();
                assert!(
                    matches!(
                        kind,
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::ConnectionAborted
                            | std::io::ErrorKind::BrokenPipe
                            | std::io::ErrorKind::InvalidData
                    ),
                    "unexpected error kind {kind:?} (msg: {e})",
                );
            }
        }

        // Make sure shutdown itself returned cleanly; this is what
        // catches a hung joiner / panicking shard.
        stop_task.await.expect("server stop task panicked");
    })
    .await;

    outer.expect("whole test exceeded 15s envelope — shutdown machinery hung");
}
