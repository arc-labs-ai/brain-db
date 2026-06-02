//! Extractor lifecycle exit integration test.
//!
//! Exercises the full extractor lifecycle end-to-end:
//! - System schema seeds `brain.entity_mentions` + `brain.basic_ner`.
//! - ENCODE a memory whose text matches the pattern extractor's
//!   English-name regex.
//! - Verify an audit row was written for the pattern extractor with
//!   `Success` status and a non-zero item count in `status_reason`.
//! - Verify the classifier extractor wrote a `Failure` audit row
//!   with the staged "runtime not wired" reason (a follow-up
//!   will flip this to a real inference path).
//! - DISABLE the pattern extractor → ENCODE again → assert only
//!   the classifier wrote an audit row for the second memory.

#![cfg(target_os = "linux")]

use brain_metadata::audit::ops::audit_by_memory;
use brain_metadata::tables::audit::extraction_status;
use brain_metadata::MetadataDb;
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, MemoryKindWire, RequestBody};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{ExtractorDisableRequest, ExtractorListRequest};
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
// Wire helpers.
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
        client_id: "phase-20-exit".into(),
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

/// Read all audit rows for a memory from the shard's metadata.redb.
/// MUST be called after `Server::stop()` — redb serialises opens
/// and the shard holds the lock while running.
fn read_audit_rows_after_stop(
    metadata_path: &std::path::Path,
    memory_id: brain_core::MemoryId,
) -> Vec<brain_metadata::tables::audit::ExtractionAudit> {
    let db = MetadataDb::open(metadata_path).expect("open metadata after stop");
    let rtxn = db.read_txn().expect("read_txn");
    audit_by_memory(&rtxn, memory_id, 100).expect("audit_by_memory")
}

// ---------------------------------------------------------------------------
// Phase exit lifecycle test.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn encode_dispatches_builtin_extractors_and_writes_audit_rows() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // 1. ENCODE a memory whose text matches the pattern extractor's
    //    "First Last" regex.
    let (opcode, body) = round_trip(
        &mut client,
        1,
        encode_request("Alice Cooper attended the planning session"),
    )
    .await;
    assert_eq!(opcode, Opcode::EncodeResp.as_u16());
    let memory_id_bytes = match body {
        ResponseBody::Encode(r) => {
            assert!(!r.was_deduplicated);
            r.memory_id
        }
        other => panic!("expected EncodeResp, got {other:?}"),
    };
    let memory_id = brain_core::MemoryId::from(memory_id_bytes);

    // 2. DISABLE the pattern extractor, then ENCODE another memory.
    let (_, _) = round_trip(
        &mut client,
        3,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: 1,
            reason: "test disable".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;

    let (_, body) = round_trip(
        &mut client,
        5,
        encode_request("Bob Smith showed up to the meeting"),
    )
    .await;
    let memory2_bytes = match body {
        ResponseBody::Encode(r) => r.memory_id,
        _ => unreachable!(),
    };
    let memory2 = brain_core::MemoryId::from(memory2_bytes);

    // 3. LIST confirms the pattern is still disabled (wire-side
    //    check before we stop the server and inspect storage).
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
    )
    .await;
    match body {
        ResponseBody::ExtractorList(r) => {
            let entity_mentions = r
                .items
                .iter()
                .find(|i| i.name == "entity_mentions")
                .unwrap();
            assert!(!entity_mentions.enabled);
        }
        _ => unreachable!(),
    }

    // 4. Stop the server so redb's exclusive lock releases; then
    //    open the metadata file directly to verify the audit
    //    rows. ENCODE's extractor dispatch is synchronous so all
    //    rows are flushed by the time the ENCODE response
    //    returned.
    server.stop().await;
    let paths = ShardPaths::at(data_dir.path().join("0"));
    let metadata_path = paths.metadata_db();

    // 4a. Memory 1 — both extractors dispatched: pattern Success
    //     with item count, classifier Failure with the staged
    //     "runtime not wired" reason.
    let rows = read_audit_rows_after_stop(&metadata_path, memory_id);
    assert_eq!(rows.len(), 2, "two extractors dispatched on memory 1");

    let pattern_row = rows
        .iter()
        .find(|r| r.extractor_id == 1)
        .expect("pattern extractor audit row");
    assert_eq!(pattern_row.status, extraction_status::SUCCESS);
    assert!(
        pattern_row.status_reason.contains("items produced"),
        "got: {:?}",
        pattern_row.status_reason
    );
    assert_eq!(pattern_row.schema_version, 1);
    assert_eq!(pattern_row.memory_id(), memory_id);

    let classifier_row = rows
        .iter()
        .find(|r| r.extractor_id == 2)
        .expect("classifier extractor audit row");
    assert_eq!(classifier_row.status, extraction_status::FAILURE);
    assert!(
        classifier_row.status_reason.contains("runtime not wired")
            || classifier_row.status_reason.contains("not loaded"),
        "expected runtime-not-wired hint, got: {:?}",
        classifier_row.status_reason
    );

    // 4b. Memory 2 — only the classifier wrote an audit row,
    //     because the pattern extractor was disabled mid-test.
    let rows2 = read_audit_rows_after_stop(&metadata_path, memory2);
    assert_eq!(rows2.len(), 1, "disabled pattern is skipped on memory 2");
    assert_eq!(rows2[0].extractor_id, 2, "only classifier dispatched");

    drop(data_dir);
}
