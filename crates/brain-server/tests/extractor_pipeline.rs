//! End-to-end ExtractorWorker smoke.
//!
//! Spawns one shard with the default extractor tick (1s) + the system
//! schema's built-in `entity_mentions` pattern extractor. Sends an
//! ENCODE through the wire, waits for the extractor to drain, and
//! asserts the entity + mention edge land on disk.

#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::time::Duration;

use brain_core::{EdgeKindRef, MemoryId, NodeRef};
use brain_metadata::pipeline_has_extracted;
use brain_metadata::tables::edge::{EdgeKey, EDGES_TABLE};
use brain_metadata::tables::entity::ENTITIES_TABLE;
use brain_metadata::MetadataDb;
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, MemoryKindWire, RequestBody};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use redb::ReadableTable;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

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
    let (frame, _) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "extractor-pipeline-tester".into(),
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
        agent_id: *Uuid::now_v7().as_bytes(),
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

fn metadata_path(data_dir: &std::path::Path) -> PathBuf {
    data_dir.join("0").join("metadata.redb")
}

/// Encode a text the built-in `entity_mentions` pattern matches, wait
/// for the extractor to tick, then assert the entity + mention edge
/// landed on disk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn encode_drives_pattern_extractor_and_writes_mention_edge() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client).await;

    let text = "Priya Patel works at Acme Corp".to_string();
    let req = EncodeRequest {
        text: text.clone(),
        context_id: 1,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: vec![],
        request_id: *Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        deduplicate: false,
    };
    let (opcode, body) = round_trip(&mut client, 1, RequestBody::Encode(req)).await;
    let memory_id = match body {
        ResponseBody::Encode(ack) => MemoryId::from(ack.memory_id),
        other => panic!("expected Encode (opcode={opcode}), got {other:?}"),
    };

    // Wait long enough for at least 3 extractor cycles (default 1s tick).
    // The shard holds an exclusive lock on metadata.redb so we can't
    // peek while it's alive — instead we sleep, then stop the server
    // (which drops the redb handle), then re-open the file from the
    // test and assert.
    tokio::time::sleep(Duration::from_secs(4)).await;
    server.stop().await;

    let mdb = metadata_path(data_dir.path());
    let db = MetadataDb::open(&mdb).expect("re-open after stop");
    let rtxn = db.read_txn().unwrap();
    assert!(
        pipeline_has_extracted(&rtxn, memory_id).unwrap_or(false),
        "pipeline audit row missing — extractor did not drain"
    );
    drop(rtxn);
    drop(db);

    // Probe the on-disk entity + mention edge.
    let db = MetadataDb::open(&mdb).unwrap();
    let rtxn = db.read_txn().unwrap();
    let entities_t = rtxn.open_table(ENTITIES_TABLE).unwrap();
    let mut priya_id_bytes: Option<[u8; 16]> = None;
    for entry in entities_t.iter().unwrap() {
        let (_, v) = entry.unwrap();
        let row = v.value();
        if row.canonical_name == "Priya Patel" {
            priya_id_bytes = Some(row.entity_id_bytes);
            break;
        }
    }
    let priya_id_bytes = priya_id_bytes.expect("Priya Patel entity must exist");
    let priya_id = brain_core::EntityId::from(priya_id_bytes);

    // Forward Mention edge.
    let edges_t = rtxn.open_table(EDGES_TABLE).unwrap();
    let prefix = NodeRef::Memory(memory_id).to_bytes();
    let mut upper = prefix.to_vec();
    upper.push(0xFF);
    let mut hit = false;
    for entry in edges_t.range(prefix.as_slice()..upper.as_slice()).unwrap() {
        let (key, _) = entry.unwrap();
        let decoded = EdgeKey::decode(key.value()).unwrap();
        if matches!(decoded.kind, EdgeKindRef::Mentions) && decoded.to == NodeRef::Entity(priya_id)
        {
            hit = true;
            break;
        }
    }
    assert!(hit, "expected Mentions edge memory → Priya Patel");

    drop(rtxn);
    drop(db);
}
