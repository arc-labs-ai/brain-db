//! Graceful shutdown preserves acknowledged writes — the "no data loss"
//! acceptance criterion at the *server* level.
//!
//! Brain's durability contract is WAL-before-acknowledge: an ENCODE that
//! returns a `memory_id` has been fsynced. This test proves that contract
//! holds across the production graceful-shutdown path — not just at the shard
//! level (covered by `shard.rs::wal_persists_across_restart`), but through the
//! full server: connection listener + admin + shards.
//!
//! Sequence:
//!   1. `start_in(dir, 1)` — a full server on a persistent data dir.
//!   2. Handshake + ENCODE several memories; collect the acknowledged ids.
//!   3. `Server::stop()` — the graceful drain (signal → listener/admin exit →
//!      shard channels close → in-shard drain: scheduler → WAL flush → arena
//!      msync → joiner returns).
//!   4. `start_in(dir, 1)` again — a brand-new server binds the same dir and
//!      replays the WAL / reseeds from the arena.
//!   5. RECALL each memory's text and assert its id comes back.
//!
//! With the harness's zero-vector stub dispatcher, semantic ranking is
//! degenerate, but the lexical (tantivy) retriever matches on real text — so a
//! cue overlapping the stored text deterministically surfaces the memory.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, RecallRequest, RequestBody,
};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use tempfile::TempDir;
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

use support_harness::start_in;

const FLAG_EOS: u8 = 1 << 7;

async fn read_one_frame<S>(stream: &mut S) -> Frame
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("header read");
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .expect("payload read");
    }
    let (frame, rest) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    debug_assert!(rest.is_empty());
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn round_trip(
    client: &mut TcpStream,
    stream_id: u32,
    req: RequestBody,
) -> (u16, ResponseBody) {
    let opcode = req.opcode().as_u16();
    send_frame(
        client,
        Frame::new(opcode, FLAG_EOS, stream_id, req.encode()),
    )
    .await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    let body = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

async fn handshake(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "durability-tester".into(),
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
    .await;
    let welcome = read_one_frame(client).await;
    assert_eq!(welcome.header.opcode_u16(), Opcode::Welcome.as_u16());

    let auth = AuthPayload {
        method: AuthMethod::None,
        agent_id,
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
    .await;
    let auth_ok = read_one_frame(client).await;
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

/// ENCODE `text`, requiring success, and return the acknowledged (durable)
/// memory_id.
async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if opcode == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => {
            panic!("encode must succeed to be a durable write: opcode={opcode} body={other:?}")
        }
    }
}

/// RECALL `cue` and return the memory_ids in the result set.
async fn recall_ids(client: &mut TcpStream, stream_id: u32, cue: &str) -> Vec<u128> {
    let req = RecallRequest {
        cue_text: cue.into(),
        subject_name: String::new(),
        max_results: 50,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Recall(req)).await;
    assert_eq!(
        opcode,
        Opcode::RecallResp.as_u16(),
        "expected RecallResp, got 0x{opcode:02x}"
    );
    match body {
        ResponseBody::Recall(r) => r.memories.iter().map(|h| h.memory_id).collect(),
        other => panic!("expected RecallResp, got {other:?}"),
    }
}

/// Retry a recall until it surfaces `wanted` or a deadline passes. After
/// restart the lexical index is repopulated asynchronously, so a recall fired
/// immediately can race ahead of it; durability is about the write surviving,
/// which it does once the index catches up. Each attempt mints a fresh
/// request_id (in `recall_ids`) so idempotency never pins an early empty.
async fn recall_ids_until_contains(
    client: &mut TcpStream,
    mut stream_id: u32,
    cue: &str,
    wanted: u128,
) -> Vec<u128> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let ids = recall_ids(client, stream_id, cue).await;
        if ids.contains(&wanted) || std::time::Instant::now() >= deadline {
            return ids;
        }
        stream_id += 2;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Acknowledged ENCODEs survive a graceful `Server::stop()` and are served by
/// a fresh server bound to the same data dir. This is the end-to-end
/// "no data loss" guarantee through the production shutdown path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acknowledged_writes_survive_graceful_shutdown_and_restart() {
    // Caller-owned dir so it outlives the first server's stop().
    let dir = TempDir::new().expect("tmp");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();

    // Distinctive phrases so the lexical retriever can re-find each one by a
    // cue that overlaps its text.
    let memories = [
        "the launch sequence checkpoint is alpha seven niner",
        "quarterly revenue beat the forecast by twelve percent",
        "the backup vault rotates its key every ninety days",
    ];
    let cues = [
        "launch sequence checkpoint alpha",
        "quarterly revenue forecast",
        "backup vault key rotation",
    ];

    // --- Server instance 1: encode + graceful shutdown ---
    let mut ids = Vec::new();
    {
        let server = start_in(dir.path(), 1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect 1");
        handshake(&mut client, agent_id).await;

        for (i, text) in memories.iter().enumerate() {
            // Client-initiated streams must be odd.
            ids.push(encode(&mut client, 1 + (i as u32) * 2, text).await);
        }
        drop(client);

        // The production graceful drain: signal → listener/admin exit → shard
        // channels close → WAL flush + arena msync → joiners return. When this
        // resolves, every acknowledged write is durable on disk.
        server.stop().await;
    }

    // --- Server instance 2: same dir, must recover every write ---
    {
        let server = start_in(dir.path(), 1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect 2");
        handshake(&mut client, agent_id).await;

        for (i, cue) in cues.iter().enumerate() {
            let got = recall_ids_until_contains(&mut client, 1 + (i as u32) * 2, cue, ids[i]).await;
            assert!(
                got.contains(&ids[i]),
                "DATA LOSS: memory {} (\"{}\") encoded before graceful shutdown was not \
                 recoverable after restart; recall for \"{}\" returned {:?}",
                ids[i],
                memories[i],
                cue,
                got,
            );
        }
        drop(client);
        server.stop().await;
    }
}

/// A graceful shutdown with no writes in flight still restarts to an empty,
/// serving state — proves the drain/restart cycle is clean on the cold path,
/// not only when there's data to flush.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graceful_shutdown_with_no_writes_restarts_clean() {
    let dir = TempDir::new().expect("tmp");
    let agent_id = *uuid::Uuid::now_v7().as_bytes();

    {
        let server = start_in(dir.path(), 1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect 1");
        handshake(&mut client, agent_id).await;
        drop(client);
        server.stop().await;
    }

    {
        let server = start_in(dir.path(), 1).await;
        let mut client = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect 2");
        handshake(&mut client, agent_id).await;
        // The shard recovered to an empty, queryable state.
        let got = recall_ids(&mut client, 1, "anything at all").await;
        assert!(
            got.is_empty(),
            "expected empty recall on a fresh dir, got {got:?}"
        );
        drop(client);
        server.stop().await;
    }
}
