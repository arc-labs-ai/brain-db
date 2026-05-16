//! Statement wire-op smoke (sub-task 17.10a).
//!
//! Drives `STATEMENT_CREATE / _GET / _SUPERSEDE / _TOMBSTONE /
//! _RETRACT / _HISTORY / _LIST` through the full data-plane stack
//! (TCP → frame codec → connection layer → shard executor →
//! brain-ops dispatch → brain-metadata statement_ops) and asserts
//! per-opcode behaviour + error paths.
//!
//! Linux-only via `target_os = "linux"` (glommio's shard executor).
//! Cross-compile verified on macOS via `cargo zigbuild --tests`.

#![cfg(target_os = "linux")]

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::knowledge::{
    EntityCreateRequest, EvidenceRefWire, StatementCreateRequest, StatementGetRequest,
    StatementHistoryRequest, StatementKindWire, StatementListRequest, StatementObjectWire,
    StatementRetractRequest, StatementSupersedeRequest, StatementTombstoneRequest,
    StatementValueWire,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::RequestBody;
use brain_protocol::response::ResponseBody;
use brain_protocol::Frame;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

#[allow(dead_code)]
#[path = "../src/admin/mod.rs"]
mod admin;
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
    let (frame, rest) = Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32)
        .expect("decode");
    debug_assert!(rest.is_empty());
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "statement-tester".into(),
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

async fn make_entity(
    client: &mut TcpStream,
    stream_id: u32,
    name: &str,
) -> [u8; 16] {
    let (op, body) = round_trip(
        client,
        stream_id,
        RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: PERSON_TYPE_ID,
            canonical_name: name.into(),
            aliases: vec![],
            attributes_blob: Vec::new(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::EntityCreateResp.as_u16(), "entity create resp");
    match body {
        ResponseBody::EntityCreate(r) => r.entity_id,
        other => panic!("expected EntityCreateResp, got {other:?}"),
    }
}

fn rid() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

/// Build a Fact create request against the built-in `brain:related_to`
/// predicate (Fact / Entity object).
fn fact_request(subject: [u8; 16], object: [u8; 16]) -> StatementCreateRequest {
    StatementCreateRequest {
        kind: StatementKindWire::Fact,
        subject,
        predicate: "brain:related_to".into(),
        object: StatementObjectWire::EntityRef(object),
        confidence: 0.9,
        evidence: EvidenceRefWire::Inline(vec![]),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 0,
        request_id: rid(),
    }
}

fn pref_request(subject: [u8; 16], value: &str) -> StatementCreateRequest {
    StatementCreateRequest {
        kind: StatementKindWire::Preference,
        subject,
        predicate: "brain:prefers".into(),
        object: StatementObjectWire::Value(StatementValueWire::Text(value.into())),
        confidence: 0.85,
        evidence: EvidenceRefWire::Inline(vec![]),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 0,
        request_id: rid(),
    }
}

fn event_request(subject: [u8; 16], when: u64) -> StatementCreateRequest {
    StatementCreateRequest {
        kind: StatementKindWire::Event,
        subject,
        predicate: "brain:scheduled".into(),
        object: StatementObjectWire::Value(StatementValueWire::Text("session".into())),
        confidence: 0.95,
        evidence: EvidenceRefWire::Inline(vec![]),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: when,
        schema_version: 0,
        request_id: rid(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn create_fact_round_trips() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let mgr = make_entity(&mut client, 3, "Manager-A").await;

    let (op, body) = round_trip(
        &mut client,
        5,
        RequestBody::StatementCreate(fact_request(priya, mgr)),
    )
    .await;
    assert_eq!(op, Opcode::StatementCreateResp.as_u16());
    let sid = match body {
        ResponseBody::StatementCreate(r) => {
            assert_eq!(r.auto_superseded, [0u8; 16], "no auto-supersede for Fact");
            assert_ne!(r.statement_id, [0u8; 16]);
            assert_eq!(
                r.chain_root, r.statement_id,
                "chain_root = id for first version"
            );
            r.statement_id
        }
        other => panic!("expected StatementCreateResp, got {other:?}"),
    };

    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementGet(StatementGetRequest {
            statement_id: sid,
            follow_supersession: false,
        }),
    )
    .await;
    match body {
        ResponseBody::StatementGet(r) => {
            assert_eq!(r.statement.statement_id, sid);
            assert_eq!(r.statement.predicate, "brain:related_to");
            assert_eq!(r.statement.confidence, 0.9);
            assert_eq!(r.statement.version, 1);
            assert!(!r.statement.tombstoned);
            assert!(!r.returned_via_supersession);
        }
        other => panic!("expected StatementGetResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_preference_auto_supersedes() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;

    let (_, body) = round_trip(
        &mut client,
        3,
        RequestBody::StatementCreate(pref_request(priya, "async meetings")),
    )
    .await;
    let p1 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::StatementCreate(pref_request(priya, "written agendas")),
    )
    .await;
    match body {
        ResponseBody::StatementCreate(r) => {
            assert_eq!(r.auto_superseded, p1, "second pref auto-supersedes first");
            assert_eq!(r.chain_root, p1, "chain_root inherits from old");
            assert_ne!(r.statement_id, p1);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_event_requires_event_at() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let mut req = event_request(priya, 0); // event_at = 0 → invalid
    req.event_at_unix_nanos = 0;

    let (op, body) = round_trip(&mut client, 3, RequestBody::StatementCreate(req)).await;
    assert_eq!(op, Opcode::Error.as_u16(), "missing event_at → ERROR");
    match body {
        ResponseBody::Error(e) => {
            assert!(
                e.message.to_lowercase().contains("event"),
                "error mentions event: {:?}",
                e.message
            );
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_unknown_predicate_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let mgr = make_entity(&mut client, 3, "M").await;
    let mut req = fact_request(priya, mgr);
    req.predicate = "user:not_registered".into();

    let (op, body) = round_trip(&mut client, 5, RequestBody::StatementCreate(req)).await;
    assert_eq!(op, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => {
            assert!(
                e.message.to_lowercase().contains("predicate"),
                "error mentions predicate: {:?}",
                e.message
            );
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn get_missing_statement_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let (op, body) = round_trip(
        &mut client,
        1,
        RequestBody::StatementGet(StatementGetRequest {
            statement_id: rid(),
            follow_supersession: false,
        }),
    )
    .await;
    assert_eq!(op, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => assert!(e.message.to_lowercase().contains("statement")),
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_returns_new_id_and_version() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let mgr_a = make_entity(&mut client, 3, "A").await;
    let mgr_b = make_entity(&mut client, 5, "B").await;

    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementCreate(fact_request(priya, mgr_a)),
    )
    .await;
    let f1 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };

    let (op, body) = round_trip(
        &mut client,
        9,
        RequestBody::StatementSupersede(StatementSupersedeRequest {
            old_statement_id: f1,
            new_statement: fact_request(priya, mgr_b),
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::StatementSupersedeResp.as_u16());
    match body {
        ResponseBody::StatementSupersede(r) => {
            assert_eq!(r.version, 2);
            assert_eq!(r.chain_root, f1);
            assert_ne!(r.new_statement_id, f1);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tombstone_returns_timestamp() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let mgr = make_entity(&mut client, 3, "M").await;
    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::StatementCreate(fact_request(priya, mgr)),
    )
    .await;
    let sid = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };

    let (op, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementTombstone(StatementTombstoneRequest {
            statement_id: sid,
            reason: 2, // UserRequest
            reason_message: "test tombstone".into(),
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::StatementTombstoneResp.as_u16());
    match body {
        ResponseBody::StatementTombstone(r) => assert!(r.tombstoned_at_unix_nanos > 0),
        other => panic!("{other:?}"),
    }

    // Subsequent GET shows tombstoned = true.
    let (_, body) = round_trip(
        &mut client,
        9,
        RequestBody::StatementGet(StatementGetRequest {
            statement_id: sid,
            follow_supersession: false,
        }),
    )
    .await;
    match body {
        ResponseBody::StatementGet(r) => {
            assert!(r.statement.tombstoned);
            assert_eq!(r.statement.tombstone_reason, 2);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn retract_returns_will_zero_hint() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let mgr = make_entity(&mut client, 3, "M").await;
    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::StatementCreate(fact_request(priya, mgr)),
    )
    .await;
    let sid = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };

    let (op, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementRetract(StatementRetractRequest {
            statement_id: sid,
            reason: 4, // ExtractorRetraction
            reason_message: "wrong extraction".into(),
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::StatementRetractResp.as_u16());
    match body {
        ResponseBody::StatementRetract(r) => {
            assert!(r.retracted_at_unix_nanos > 0);
            assert!(r.will_zero_at_unix_nanos > r.retracted_at_unix_nanos);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn history_returns_chain_in_version_order() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;

    let (_, body) = round_trip(
        &mut client,
        3,
        RequestBody::StatementCreate(pref_request(priya, "v1")),
    )
    .await;
    let p1 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };
    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::StatementCreate(pref_request(priya, "v2")),
    )
    .await;
    let p2 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementCreate(pref_request(priya, "v3")),
    )
    .await;
    let p3 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };

    let (op, body) = round_trip(
        &mut client,
        9,
        RequestBody::StatementHistory(StatementHistoryRequest {
            anchor_id: p1,
            include_tombstoned: false,
        }),
    )
    .await;
    assert_eq!(op, Opcode::StatementHistoryResp.as_u16());
    match body {
        ResponseBody::StatementHistory(r) => {
            assert_eq!(r.items.len(), 3);
            assert_eq!(r.items[0].statement_id, p1);
            assert_eq!(r.items[1].statement_id, p2);
            assert_eq!(r.items[2].statement_id, p3);
            assert_eq!(r.chain_root, p1);
            assert_eq!(r.total_versions, 3);
            assert!(r.is_final);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_subject_predicate_filter() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let priya = make_entity(&mut client, 1, "Priya").await;
    let _ = round_trip(
        &mut client,
        3,
        RequestBody::StatementCreate(pref_request(priya, "v1")),
    )
    .await;
    let _ = round_trip(
        &mut client,
        5,
        RequestBody::StatementCreate(pref_request(priya, "v2")),
    )
    .await;

    let (op, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementList(StatementListRequest {
            subject: priya,
            predicate: "brain:prefers".into(),
            kind: 2, // Preference
            min_confidence: 0.0,
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            only_current: true,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::StatementListResp.as_u16());
    match body {
        ResponseBody::StatementList(r) => {
            assert_eq!(r.items.len(), 1, "current_only: just the latest");
            assert!(r.is_final);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_limit_zero_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let (op, body) = round_trip(
        &mut client,
        1,
        RequestBody::StatementList(StatementListRequest {
            subject: [0u8; 16],
            predicate: String::new(),
            kind: 0,
            min_confidence: 0.0,
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            only_current: false,
            include_tombstoned: false,
            limit: 0,
            cursor: Vec::new(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => assert!(e.message.to_lowercase().contains("limit")),
        other => panic!("{other:?}"),
    }

    server.stop().await;
}
