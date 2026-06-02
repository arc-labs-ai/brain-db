//! Lexical pipeline exit integration test.
//!
//! Exercises the full lexical pipeline end-to-end via the wire:
//! - ENCODE a memory whose text contains a brain-analyzer
//!   protected token (`ACME-1247`).
//! - Stop the server so the indexer drain task commits + drops
//!   its writer (the drop-of-Sender path in `run_loop`).
//! - Open `memory_text.tantivy/` from disk and query through
//!   the public `LexicalRetriever` surface — the protected
//!   token must surface the memory's id.
//!
//! Linux-only because the shard runtime uses Glommio.

#![cfg(target_os = "linux")]

use brain_index::{
    IndexStatus, LexicalQuery, LexicalRetriever, LexicalRetrieverConfig, LexicalScope,
    RankedItemId, TantivyLexicalRetriever, TantivyShard,
};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RequestBody,
};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_storage::ShardPaths;
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
use tempfile::TempDir;

const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// Wire helpers — copied from the extractors exit test.
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Frame
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("header");
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .expect("payload");
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

async fn complete_handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "phase-22-exit".into(),
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
    .await;
    let auth_ok = read_one_frame(client).await;
    assert_eq!(auth_ok.header.opcode_u16(), Opcode::AuthOk.as_u16());
}

async fn round_trip(
    client: &mut TcpStream,
    stream_id: u32,
    req: RequestBody,
) -> (u16, ResponseBody) {
    let opcode = req.opcode().as_u16();
    let payload = req.encode();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, payload)).await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    let body = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

fn encode_request(text: &str) -> RequestBody {
    RequestBody::Encode(EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    })
}

fn forget_request(memory_id: u128) -> RequestBody {
    RequestBody::Forget(ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
    })
}

/// Open the shard's `memory_text.tantivy/` after the server has
/// stopped and query through the public retriever.
fn retrieve_memory_hits(shard_dir: &std::path::Path, term: &str) -> Vec<RankedItemId> {
    let startup = TantivyShard::open(shard_dir).expect("open tantivy post-stop");
    assert!(
        matches!(startup.memory_status, IndexStatus::Ready),
        "memory_text must be Ready after server stop; got {:?}",
        startup.memory_status,
    );
    let retriever = TantivyLexicalRetriever::new(startup.shard).expect("retriever");
    retriever
        .retrieve(
            &LexicalQuery {
                terms: vec![term.into()],
                ..Default::default()
            },
            LexicalScope::MemoryText,
            &LexicalRetrieverConfig::default(),
        )
        .expect("retrieve")
        .into_iter()
        .map(|r| r.id)
        .collect()
}

// ---------------------------------------------------------------------------
// Phase-exit lifecycle tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn encode_then_lexical_retrieve_returns_hit() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        encode_request("ticket ACME-1247 reproduces under heavy load"),
    )
    .await;
    assert_eq!(opcode, Opcode::EncodeResp.as_u16());
    let memory_id = match body {
        ResponseBody::Encode(r) => brain_core::MemoryId::from(r.memory_id),
        other => panic!("expected EncodeResp, got {other:?}"),
    };

    // Server::stop drops the per-shard channels; the drain task
    // sees Sender disconnected, commits the final batch, exits.
    server.stop().await;

    let paths = ShardPaths::at(data_dir.path().join("0"));
    let _ = paths.memory_text_tantivy(); // sanity

    let hits = retrieve_memory_hits(&data_dir.path().join("0"), "acme-1247");
    assert_eq!(
        hits.len(),
        1,
        "ACME-1247 must surface after ENCODE; got {hits:?}"
    );
    match hits[0] {
        RankedItemId::Memory(id) => assert_eq!(id, memory_id),
        other => panic!("expected Memory id, got {other:?}"),
    }

    drop(data_dir);
}

#[tokio::test(flavor = "current_thread")]
async fn forget_removes_memory_from_lexical_index() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, body) = round_trip(
        &mut client,
        1,
        encode_request("forgettable note about pineapples"),
    )
    .await;
    let memory_id_bytes = match body {
        ResponseBody::Encode(r) => r.memory_id,
        _ => unreachable!(),
    };

    let (_, _) = round_trip(&mut client, 3, forget_request(memory_id_bytes)).await;

    server.stop().await;

    let hits = retrieve_memory_hits(&data_dir.path().join("0"), "pineappl");
    assert!(
        hits.is_empty(),
        "FORGET must remove the doc from memory_text; got {hits:?}",
    );

    drop(data_dir);
}
