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
use brain_protocol::envelope::request::{EncodeRequest, ForgetMode, ForgetRequest, RequestBody};
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

async fn complete_handshake(client: &mut TcpStream, token: &[u8]) {
    let hello = HelloPayload {
        client_id: "phase-22-exit".into(),
        supported_versions: vec![brain_protocol::VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_connection_token: None,
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
        method: AuthMethod::Token,
        credentials: AuthCredentials::Token(token.to_vec()),
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
        session_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    })
}

fn forget_request(memory_id: u128) -> RequestBody {
    RequestBody::Forget(ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        act_as: None,
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
    complete_handshake(&mut client, &server.token).await;

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
    complete_handshake(&mut client, &server.token).await;

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

/// The live tantivy rebuild preserves already-indexed data and resumes
/// indexing new writes — the end-to-end invariant-#7 guarantee for a hot
/// lexical rebuild.
///
/// 1. ENCODE a memory (its rows land in authoritative redb at ack).
/// 2. Drive the shard's live rebuild (`ShardHandle::rebuild_index`) for a
///    tantivy target — quiesce indexers, rebuild both lexical indexes from
///    redb, reopen, swap the retriever, resume the indexers.
/// 3. ENCODE a second memory AFTER the rebuild — proves the resumed
///    indexer writes to the new index.
/// 4. Stop and read the on-disk index: both memories are present.
#[tokio::test(flavor = "current_thread")]
async fn live_tantivy_rebuild_preserves_data_and_resumes_indexing() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client, &server.token).await;

    // 1. Pre-rebuild ENCODE. WaitMode::Ack ⇒ committed to redb, which is
    //    what the rebuild reconstructs from.
    let (_, body) = round_trip(
        &mut client,
        1,
        encode_request("ticket ACME-1247 reproduces under load"),
    )
    .await;
    let mem1 = match body {
        ResponseBody::Encode(r) => brain_core::MemoryId::from(r.memory_id),
        other => panic!("expected EncodeResp, got {other:?}"),
    };

    // 2. Live rebuild of the lexical indexes on shard 0.
    server.handles[0]
        .rebuild_index(shard::rebuild::RebuildTarget::TantivyMemory)
        .await
        .expect("live tantivy rebuild succeeds");

    // 3. Post-rebuild ENCODE — must be indexed by the resumed indexer.
    let (_, body2) = round_trip(
        &mut client,
        3,
        encode_request("followup SPROCKET-9 diagnostic run"),
    )
    .await;
    let mem2 = match body2 {
        ResponseBody::Encode(r) => brain_core::MemoryId::from(r.memory_id),
        other => panic!("expected EncodeResp, got {other:?}"),
    };

    // 4. Stop (flushes the resumed indexer) and read the on-disk index.
    server.stop().await;

    let acme = retrieve_memory_hits(&data_dir.path().join("0"), "acme-1247");
    assert_eq!(
        acme,
        vec![RankedItemId::Memory(mem1)],
        "pre-rebuild memory survives the live rebuild (rebuilt from redb)",
    );
    let sprocket = retrieve_memory_hits(&data_dir.path().join("0"), "sprocket-9");
    assert_eq!(
        sprocket,
        vec![RankedItemId::Memory(mem2)],
        "post-rebuild memory indexed by the resumed indexer",
    );

    drop(data_dir);
}
