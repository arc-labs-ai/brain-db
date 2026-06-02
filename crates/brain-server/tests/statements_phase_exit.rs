//! Statement lifecycle exit integration test.
//!
//! Exercises the full statement lifecycle end-to-end over the wire:
//! create Fact then contradiction → supersede Fact → create Preference
//! then auto-supersede → list current → history → create Event →
//! tombstone → retract.
//!
//! Companion to `statement_wire.rs`: individual
//! op smoke + error paths. This test focuses on **lifecycle ordering**
//! — that the operations compose correctly across a single subject's
//! history, in the order a real operator would issue them.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    EntityCreateRequest, EvidenceRefWire, StatementCreateRequest, StatementGetRequest,
    StatementHistoryRequest, StatementKindWire, StatementListRequest, StatementObjectWire,
    StatementRetractRequest, StatementSupersedeRequest, StatementTombstoneRequest,
    StatementValueWire,
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
        client_id: "phase-exit-statement".into(),
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
    let _welcome = read_one_frame(client).await;
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
    let _auth_ok = read_one_frame(client).await;
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
    let body = ResponseBody::decode(
        Opcode::from_u16(resp.header.opcode_u16()).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp.header.opcode_u16(), body)
}

async fn make_entity(client: &mut TcpStream, stream_id: u32, name: &str) -> [u8; 16] {
    let (_, body) = round_trip(
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
    match body {
        ResponseBody::EntityCreate(r) => r.entity_id,
        other => panic!("expected EntityCreate, got {other:?}"),
    }
}

fn rid() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

fn fact_req(subject: [u8; 16], object: [u8; 16]) -> StatementCreateRequest {
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

fn pref_req(subject: [u8; 16], value: &str) -> StatementCreateRequest {
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

fn event_req(subject: [u8; 16], when: u64) -> StatementCreateRequest {
    StatementCreateRequest {
        kind: StatementKindWire::Event,
        subject,
        predicate: "brain:scheduled".into(),
        object: StatementObjectWire::Value(StatementValueWire::Text("planning session".into())),
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
// Lifecycle.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn statement_lifecycle_end_to_end() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // Step 1 — create the subject + two manager entities.
    let priya = make_entity(&mut client, 1, "Priya").await;
    let mgr_a = make_entity(&mut client, 3, "Mgr-A").await;
    let mgr_b = make_entity(&mut client, 5, "Mgr-B").await;

    // Step 2 — create a Fact.
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::StatementCreate(fact_req(priya, mgr_a)),
    )
    .await;
    let f1 = match body {
        ResponseBody::StatementCreate(r) => {
            assert_eq!(r.chain_root, r.statement_id);
            r.statement_id
        }
        other => panic!("{other:?}"),
    };

    // Step 3 — create a contradictory Fact (same subject + predicate,
    // different object). Both must persist; the second insert
    // overwrites the by_subject index entry, but the row remains
    // reachable by id.
    let (_, body) = round_trip(
        &mut client,
        9,
        RequestBody::StatementCreate(fact_req(priya, mgr_b)),
    )
    .await;
    let f2 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };
    assert_ne!(f1, f2);

    // Step 4 — both Facts reachable via GET (both primary rows
    // exist; the by_subject single-value index limitation is
    // a v1 trade-off).
    let (_, body) = round_trip(
        &mut client,
        11,
        RequestBody::StatementGet(StatementGetRequest {
            statement_id: f1,
            follow_supersession: false,
        }),
    )
    .await;
    match body {
        ResponseBody::StatementGet(r) => assert_eq!(r.statement.statement_id, f1),
        other => panic!("{other:?}"),
    }
    let (_, body) = round_trip(
        &mut client,
        13,
        RequestBody::StatementGet(StatementGetRequest {
            statement_id: f2,
            follow_supersession: false,
        }),
    )
    .await;
    match body {
        ResponseBody::StatementGet(r) => assert_eq!(r.statement.statement_id, f2),
        other => panic!("{other:?}"),
    }

    // Step 5 — explicit supersede on f2 to settle the contradiction.
    let (_, body) = round_trip(
        &mut client,
        15,
        RequestBody::StatementSupersede(StatementSupersedeRequest {
            old_statement_id: f2,
            new_statement: fact_req(priya, mgr_a), // settle on mgr_a
            request_id: rid(),
        }),
    )
    .await;
    let f3 = match body {
        ResponseBody::StatementSupersede(r) => {
            assert_eq!(r.version, 2);
            assert_eq!(r.chain_root, f2);
            r.new_statement_id
        }
        other => panic!("{other:?}"),
    };

    // Step 6 — create a Preference.
    let (_, body) = round_trip(
        &mut client,
        17,
        RequestBody::StatementCreate(pref_req(priya, "async meetings")),
    )
    .await;
    let p1 = match body {
        ResponseBody::StatementCreate(r) => {
            assert_eq!(r.auto_superseded, [0u8; 16]);
            r.statement_id
        }
        other => panic!("{other:?}"),
    };

    // Step 7 — second Preference auto-supersedes the first.
    let (_, body) = round_trip(
        &mut client,
        19,
        RequestBody::StatementCreate(pref_req(priya, "written agendas")),
    )
    .await;
    let p2 = match body {
        ResponseBody::StatementCreate(r) => {
            assert_eq!(r.auto_superseded, p1);
            assert_eq!(r.chain_root, p1);
            r.statement_id
        }
        other => panic!("{other:?}"),
    };

    // Step 8 — list current Preferences for priya. Exactly one.
    let (_, body) = round_trip(
        &mut client,
        21,
        RequestBody::StatementList(StatementListRequest {
            subject: priya,
            predicate: "brain:prefers".into(),
            kind: 2,
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
    match body {
        ResponseBody::StatementList(r) => {
            assert_eq!(r.items.len(), 1);
            assert_eq!(r.items[0].statement_id, p2);
            assert_eq!(r.items[0].version, 2);
        }
        other => panic!("{other:?}"),
    }

    // Step 9 — Preference chain history (2 entries).
    let (_, body) = round_trip(
        &mut client,
        23,
        RequestBody::StatementHistory(StatementHistoryRequest {
            anchor_id: p1,
            include_tombstoned: false,
        }),
    )
    .await;
    match body {
        ResponseBody::StatementHistory(r) => {
            assert_eq!(r.items.len(), 2);
            assert_eq!(r.items[0].statement_id, p1);
            assert_eq!(r.items[1].statement_id, p2);
            assert_eq!(r.chain_root, p1);
            assert_eq!(r.total_versions, 2);
        }
        other => panic!("{other:?}"),
    }

    // Step 10 — create an Event.
    let event_at = 1_700_000_000_000_000_000u64;
    let (_, body) = round_trip(
        &mut client,
        25,
        RequestBody::StatementCreate(event_req(priya, event_at)),
    )
    .await;
    let e1 = match body {
        ResponseBody::StatementCreate(r) => r.statement_id,
        other => panic!("{other:?}"),
    };

    // Step 11 — tombstone the Event.
    let (_, body) = round_trip(
        &mut client,
        27,
        RequestBody::StatementTombstone(StatementTombstoneRequest {
            statement_id: e1,
            reason: 2,
            reason_message: "operator tombstone".into(),
            request_id: rid(),
        }),
    )
    .await;
    match body {
        ResponseBody::StatementTombstone(r) => assert!(r.tombstoned_at_unix_nanos > 0),
        other => panic!("{other:?}"),
    }
    let (_, body) = round_trip(
        &mut client,
        29,
        RequestBody::StatementGet(StatementGetRequest {
            statement_id: e1,
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

    // Step 12 — retract one Fact.
    let (_, body) = round_trip(
        &mut client,
        31,
        RequestBody::StatementRetract(StatementRetractRequest {
            statement_id: f3,
            reason: 4,
            reason_message: "retract test".into(),
            request_id: rid(),
        }),
    )
    .await;
    match body {
        ResponseBody::StatementRetract(r) => {
            assert!(r.will_zero_at_unix_nanos > r.retracted_at_unix_nanos);
        }
        other => panic!("{other:?}"),
    }

    // Step 13 — final list across all kinds for priya (just verify
    // it returns without error; exact counts depend on by_subject
    // index overwrites for contradictory Facts).
    let (op, body) = round_trip(
        &mut client,
        33,
        RequestBody::StatementList(StatementListRequest {
            subject: priya,
            predicate: String::new(),
            kind: 0,
            min_confidence: 0.0,
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            only_current: false,
            include_tombstoned: true,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::StatementListResp.as_u16());
    match body {
        ResponseBody::StatementList(r) => {
            assert!(r.is_final);
            assert!(!r.items.is_empty());
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}
