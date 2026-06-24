//! FORGET cascade, end-to-end through the running server.
//!
//! The acceptance criterion: "FORGET cascade: statements depending on a
//! forgotten memory get their evidence list updated; orphaned statements get
//! tombstoned." The cascade primitive (`brain-metadata`) and the worker
//! (`brain-workers`) are unit-tested in isolation, but until the worker is
//! actually spawned by the shard and fed by the writer, a real FORGET would
//! tombstone the memory and leave dependent statements untouched. This test
//! exercises the wired path: it drives ENCODE → STATEMENT_CREATE (citing the
//! memory as its sole evidence) → FORGET through the full data plane and waits
//! for the asynchronous cascade worker to tombstone the now-orphaned statement.
//!
//! The cascade runs on the per-shard worker scheduler (≈1 s tick), so the
//! assertion polls STATEMENT_GET rather than expecting an immediate effect.

#![cfg(target_os = "linux")]

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

async fn handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "forget-cascade".into(),
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
    assert_eq!(
        read_one_frame(client).await.header.opcode_u16(),
        Opcode::Welcome.as_u16()
    );

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
    assert_eq!(
        read_one_frame(client).await.header.opcode_u16(),
        Opcode::AuthOk.as_u16()
    );
}

async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: rid(),
        txn_id: None,
        occurred_at_unix_nanos: None,
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
        request_id: rid(),
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

/// Create a Fact whose sole evidence is `memory_id`. Returns the statement id.
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
        // `related_to` is a seeded relation type, not a predicate; as a Fact
        // predicate it lives in an open-vocab user namespace, interned on use.
        predicate: "app:related_to".into(),
        object: StatementObjectWire::EntityRef(object),
        confidence: 0.9,
        evidence: EvidenceRefWire::Inline(vec![memory_id.to_be_bytes()]),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 0,
        request_id: rid(),
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

async fn forget(client: &mut TcpStream, stream_id: u32, memory_id: u128) {
    let req = ForgetRequest {
        memory_id,
        mode: ForgetMode::Hard,
        request_id: rid(),
        txn_id: None,
    };
    let (op, body) = round_trip(client, stream_id, RequestBody::Forget(req)).await;
    assert_eq!(op, Opcode::ForgetResp.as_u16(), "forget failed: {body:?}");
}

/// `true` once the statement reports `tombstoned`.
async fn statement_tombstoned(client: &mut TcpStream, stream_id: u32, stmt_id: [u8; 16]) -> bool {
    let req = StatementGetRequest {
        statement_id: stmt_id,
        follow_supersession: false,
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

// ---------------------------------------------------------------------------
// Test
// ---------------------------------------------------------------------------

/// Forgetting a statement's only-evidence memory cascades: the orphaned
/// statement is tombstoned by the (now-wired) cascade worker. Single shard so
/// the statement and its evidence memory are collocated, exercising the
/// in-shard cascade path.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn forget_cascades_tombstone_to_orphaned_statement() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake(&mut client).await;

    // The memory that will back the statement's only evidence.
    let memory_id = encode(&mut client, 1, "Priya works with the platform team").await;

    let priya = create_entity(&mut client, 3, "Priya-cascade").await;
    let team = create_entity(&mut client, 5, "Platform-cascade").await;
    let stmt_id = create_statement_citing(&mut client, 7, priya, team, memory_id).await;

    // Precondition: the statement is live before the FORGET.
    assert!(
        !statement_tombstoned(&mut client, 9, stmt_id).await,
        "statement should be live before its evidence is forgotten"
    );

    // Forget the only memory backing the statement's evidence.
    forget(&mut client, 11, memory_id).await;

    // The cascade is asynchronous (worker scheduler ≈1 s tick). Poll until the
    // orphaned statement is tombstoned, up to a generous ceiling.
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
        "FORGET cascade did not tombstone the orphaned statement within 15s — \
         the cascade worker is not running or not fed by the writer"
    );

    server.stop().await;
}
