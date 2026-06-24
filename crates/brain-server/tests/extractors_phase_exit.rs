//! Extractor lifecycle exit integration test.
//!
//! Exercises the extractor lifecycle end-to-end:
//! - System schema seeds `brain.entity_mentions`, `brain.gliner`,
//!   `brain.llm_predicate`.
//! - ENCODE a memory whose text matches the pattern extractor's
//!   English-name regex.
//! - Extraction is async (the per-shard worker drains the queue), so the
//!   test settles before reading the audit table.
//! - Verify the worker wrote a pipeline audit row whose pattern tier RAN and
//!   produced at least one entity.
//! - DISABLE the pattern extractor → ENCODE again → assert the wire `LIST`
//!   reports it disabled, and the second memory still gets a pipeline audit
//!   row from the remaining tiers.
//!
//! Storage note: the worker records ONE [`ExtractorPipelineAuditEntry`] per
//! memory (via `record_extracted`), aggregating all tiers into per-tier
//! status bytes + a combined item count. It does NOT write the legacy
//! per-extractor `ExtractionAudit` rows (`audit_write` is test/bench-only).
//! Because the pattern tier holds more than one extractor (entity_mentions +
//! temporal_expressions) and the classifier tier (GLiNER) can also surface
//! entities, the aggregate row can't isolate a single disabled extractor's
//! contribution — so the per-extractor disable is asserted at the wire level
//! (`LIST`), and storage only confirms the pipeline ran for each memory.

#![cfg(target_os = "linux")]

use brain_metadata::{
    pipeline_audit_entry, pipeline_status, tier_status, ExtractorPipelineAuditEntry, MetadataDb,
};
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, RequestBody};
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
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
    })
}

/// Read a memory's pipeline audit row from the shard's metadata.redb.
/// MUST be called after `Server::stop()` — redb serialises opens and the
/// shard holds the lock while running.
fn read_pipeline_audit_after_stop(
    metadata_path: &std::path::Path,
    memory_id: brain_core::MemoryId,
) -> Option<ExtractorPipelineAuditEntry> {
    let db = MetadataDb::open(metadata_path).expect("open metadata after stop");
    let rtxn = db.read_txn().expect("read_txn");
    pipeline_audit_entry(&rtxn, memory_id).expect("pipeline_audit_entry")
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

    // 3. LIST confirms the pattern extractor is disabled. This is the
    //    authoritative check of the per-extractor disable: the aggregate
    //    pipeline audit row can't isolate one extractor inside a tier.
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

    // 4. Extraction is asynchronous: ENCODE enqueues the memory, and the
    //    per-shard extractor worker drains the queue on its interval (~1s),
    //    writing the pipeline audit row. Give it time to run before stopping
    //    the server and reading the audit table — redb serialises opens, so
    //    the table can only be read once the shard releases its lock at
    //    shutdown, which is why we settle here rather than poll.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    server.stop().await;
    let paths = ShardPaths::at(data_dir.path().join("0"));
    let metadata_path = paths.metadata_db();

    // 4a. Memory 1 — the worker ran every enabled tier and committed one
    //     pipeline audit row. The pattern tier RAN and the deterministic
    //     entity_mentions regex matched "Alice Cooper", so the combined item
    //     count carries at least one entity. The classifier / llm tiers may
    //     or may not add items depending on which models are present, so we
    //     assert only the deterministic pattern outcome.
    let entry = read_pipeline_audit_after_stop(&metadata_path, memory_id)
        .expect("extractor worker must write a pipeline audit row for memory 1");
    assert!(
        matches!(
            entry.status,
            pipeline_status::SUCCESS | pipeline_status::PARTIAL_FAILURE
        ),
        "memory 1 pipeline must reach a committed outcome, got status {} reason {:?}",
        entry.status,
        entry.status_reason,
    );
    assert_eq!(
        entry.tier_pattern,
        tier_status::RAN,
        "pattern tier must have run for memory 1",
    );
    assert!(
        entry.item_counts.entities >= 1,
        "pattern extractor must surface at least one entity for memory 1, got {:?}",
        entry.item_counts,
    );
    assert_eq!(entry.memory_id(), memory_id);

    // 4b. Memory 2 — the worker still ran (the remaining tiers process every
    //     encode), so a pipeline audit row exists. The entity_mentions
    //     disable itself is verified at the wire level in step 3.
    let entry2 = read_pipeline_audit_after_stop(&metadata_path, memory2)
        .expect("extractor worker must write a pipeline audit row for memory 2");
    assert_eq!(entry2.memory_id(), memory2);

    drop(data_dir);
}
