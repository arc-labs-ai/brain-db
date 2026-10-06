//! Admin memory-restore (FORGET soft-cascade revert), end-to-end.
//!
//! Exercises the acked production trigger wired on top of the revert
//! engine: `POST /v1/memories/{id}/restore` un-tombstones a soft-forgotten
//! memory (WAL-durable) and enqueues the cascade revert so a statement the
//! forward cascade orphaned is re-attached.
//!
//! The cascade runs on the per-shard worker scheduler (≈1 s tick), so the
//! assertions poll rather than expecting an immediate effect.

#![cfg(target_os = "linux")]

use std::net::SocketAddr;
use std::time::Duration;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    EncodeRequest, EntityCreateRequest, EvidenceRefWire, ForgetMode, ForgetRequest,
    StatementCreateRequest, StatementGetRequest, StatementKindWire, StatementObjectWire,
};
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
const PERSON_TYPE_ID: u32 = 1;
const ADMIN_TOKEN: &str = "test-admin-token";

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
        Opcode::from_u16(resp_opcode).expect("opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

fn rid() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

async fn handshake(client: &mut TcpStream, token: &[u8]) {
    let hello = HelloPayload {
        client_id: "admin-restore".into(),
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
    assert_eq!(
        read_one_frame(client).await.header.opcode_u16(),
        Opcode::Welcome.as_u16()
    );

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
    assert_eq!(
        read_one_frame(client).await.header.opcode_u16(),
        Opcode::AuthOk.as_u16()
    );
}

async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        session_id: 0,
        request_id: rid(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    };
    let (op, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if op == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => panic!("encode failed: op={op} body={other:?}"),
    }
}

async fn create_entity(client: &mut TcpStream, stream_id: u32, name: &str) -> [u8; 16] {
    let req = EntityCreateRequest {
        entity_type_id: PERSON_TYPE_ID,
        canonical_name: name.into(),
        aliases: vec![],
        attributes_blob: Vec::new(),
        session_id: 0,
        request_id: rid(),
        act_as: None,
    };
    let (op, body) = round_trip(client, stream_id, RequestBody::EntityCreate(req)).await;
    assert_eq!(
        op,
        Opcode::EntityCreateResp.as_u16(),
        "entity create failed: {body:?}"
    );
    match body {
        ResponseBody::EntityCreate(r) => r.entity_id,
        other => panic!("expected EntityCreate, got {other:?}"),
    }
}

/// Create a Fact whose sole evidence is `memory_id`. Returns the id.
async fn create_statement_citing(
    client: &mut TcpStream,
    stream_id: u32,
    subject: [u8; 16],
    object: [u8; 16],
    memory_id: u128,
) -> [u8; 16] {
    let req = StatementCreateRequest {
        kind: StatementKindWire::Fact,
        subject,
        predicate: "app:related_to".into(),
        object: StatementObjectWire::EntityRef(object),
        confidence: 0.9,
        evidence: EvidenceRefWire::Inline(vec![memory_id.to_be_bytes()]),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 0,
        session_id: 0,
        request_id: rid(),
        act_as: None,
    };
    let (op, body) = round_trip(client, stream_id, RequestBody::StatementCreate(req)).await;
    assert_eq!(
        op,
        Opcode::StatementCreateResp.as_u16(),
        "statement create failed: {body:?}"
    );
    match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("expected StatementCreate, got {other:?}"),
    }
}

async fn forget(client: &mut TcpStream, stream_id: u32, memory_id: u128, mode: ForgetMode) {
    let req = ForgetRequest {
        memory_id,
        mode,
        request_id: rid(),
        txn_id: None,
        act_as: None,
    };
    let (op, body) = round_trip(client, stream_id, RequestBody::Forget(req)).await;
    assert_eq!(op, Opcode::ForgetResp.as_u16(), "forget failed: {body:?}");
}

async fn statement_tombstoned(client: &mut TcpStream, stream_id: u32, stmt_id: [u8; 16]) -> bool {
    let req = StatementGetRequest {
        statement_id: stmt_id,
        follow_supersession: false,
        act_as: None,
    };
    let (op, body) = round_trip(client, stream_id, RequestBody::StatementGet(req)).await;
    assert_eq!(
        op,
        Opcode::StatementGetResp.as_u16(),
        "statement get failed: {body:?}"
    );
    match body {
        ResponseBody::StatementGet(r) => r.statement.tombstoned,
        other => panic!("expected StatementGet, got {other:?}"),
    }
}

/// Single-shot authed admin POST (empty body). Returns (status, body).
async fn admin_post(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).await.expect("connect admin");
    let req = format!(
        "POST {path} HTTP/1.1\r\nhost: localhost\r\nauthorization: Bearer {ADMIN_TOKEN}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
    );
    stream.write_all(req.as_bytes()).await.expect("send");
    stream.flush().await.expect("flush");
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");
    let response = String::from_utf8_lossy(&buf).into_owned();
    let first_line = response.lines().next().unwrap_or("");
    let code = first_line
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, b)| b.to_owned())
        .unwrap_or_default();
    (code, body)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Happy path: a soft FORGET tombstones the orphaned statement; the admin
/// restore un-tombstones the memory and enqueues the revert cascade, which
/// re-attaches (un-tombstones) the statement. Two shards exercise the
/// admin fan-out — only the owning shard restores; the other reports a
/// clean miss, and the merged response is `200 OK "restored"`.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_restore_untombstones_memory_and_reverts_cascade() {
    let server = start(2).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client, &server.token).await;

    let memory_id = encode(&mut client, 1, "Priya works with the platform team").await;
    let priya = create_entity(&mut client, 3, "Priya-restore").await;
    let team = create_entity(&mut client, 5, "Platform-restore").await;
    let stmt_id = create_statement_citing(&mut client, 7, priya, team, memory_id).await;

    assert!(
        !statement_tombstoned(&mut client, 9, stmt_id).await,
        "statement should be live before its evidence is forgotten"
    );

    // Soft forget → forward cascade tombstones the orphaned statement.
    forget(&mut client, 11, memory_id, ForgetMode::Soft).await;
    let mut tombstoned = false;
    for i in 0..60u32 {
        if statement_tombstoned(&mut client, 13 + i * 2, stmt_id).await {
            tombstoned = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        tombstoned,
        "soft FORGET cascade did not tombstone the orphaned statement within 15s"
    );

    // Admin restore — un-tombstone the memory + enqueue the revert cascade.
    let path = format!("/v1/memories/{memory_id}/restore?namespace=test");
    let (code, body) = admin_post(server.admin_addr, &path).await;
    assert_eq!(code, 200, "restore should be 200 OK: {body}");
    assert!(body.contains("\"outcome\":\"restored\""), "body: {body}");
    assert!(body.contains("\"restored\":true"), "body: {body}");

    // The revert cascade re-attaches the statement (un-tombstones it).
    let mut reverted = false;
    for i in 0..60u32 {
        if !statement_tombstoned(&mut client, 201 + i * 2, stmt_id).await {
            reverted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert!(
        reverted,
        "restore did not un-tombstone the dependent statement within 15s — \
         the revert cascade is not running or not enqueued by the restore trigger"
    );

    server.stop().await;
}

/// A hard-forgotten memory is irreversible: restore must refuse with 409.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn admin_restore_rejects_hard_forgotten() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client, &server.token).await;

    let memory_id = encode(&mut client, 1, "ephemeral secret").await;
    forget(&mut client, 3, memory_id, ForgetMode::Hard).await;

    let path = format!("/v1/memories/{memory_id}/restore?namespace=test");
    let (code, body) = admin_post(server.admin_addr, &path).await;
    assert_eq!(code, 409, "hard-forgotten restore should be 409: {body}");
    assert!(
        body.contains("\"outcome\":\"hard_forgotten\""),
        "body: {body}"
    );
    assert!(body.contains("\"restored\":false"), "body: {body}");

    server.stop().await;
}

/// An unknown memory id resolves to 404.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_restore_unknown_memory_is_404() {
    let server = start(1).await;
    // A plausible-but-absent id (shard 0, slot 9999).
    let missing = brain_core::MemoryId::pack(0, 9999, 1).raw();
    let path = format!("/v1/memories/{missing}/restore?namespace=test");
    let (code, body) = admin_post(server.admin_addr, &path).await;
    assert_eq!(code, 404, "unknown memory restore should be 404: {body}");
    assert!(body.contains("\"outcome\":\"not_found\""), "body: {body}");
    server.stop().await;
}

/// Missing namespace query param is a 400.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn admin_restore_missing_namespace_is_400() {
    let server = start(1).await;
    let some_id = brain_core::MemoryId::pack(0, 1, 1).raw();
    let path = format!("/v1/memories/{some_id}/restore");
    let (code, _body) = admin_post(server.admin_addr, &path).await;
    assert_eq!(code, 400, "missing namespace should be 400");
    server.stop().await;
}
