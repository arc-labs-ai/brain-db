//! Relation wire-op smoke (sub-task 18.9a).
//!
//! Drives `RELATION_CREATE / _GET / _SUPERSEDE / _TOMBSTONE /
//! _LIST_FROM / _LIST_TO / _TRAVERSE` through the full data-plane
//! stack.
//!
//! Linux-only via `target_os = "linux"`.

#![cfg(target_os = "linux")]

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::knowledge::statement_req::EvidenceRefWire;
use brain_protocol::knowledge::{
    EntityCreateRequest, RelationCreateRequest, RelationGetRequest, RelationListFromRequest,
    RelationListToRequest, RelationSupersedeRequest, RelationTombstoneRequest,
    RelationTraverseRequest,
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
        client_id: "relation-tester".into(),
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
    let _ = read_one_frame(client).await;
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
    let _ = read_one_frame(client).await;
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
        other => panic!("expected EntityCreateResp, got {other:?}"),
    }
}

fn rid() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

fn create_request(
    relation_type: &str,
    from: [u8; 16],
    to: [u8; 16],
) -> RelationCreateRequest {
    RelationCreateRequest {
        relation_type: relation_type.into(),
        from_entity: from,
        to_entity: to,
        properties_blob: Vec::new(),
        evidence: EvidenceRefWire::Inline(vec![]),
        extractor_id: 0,
        confidence: 0.9,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        request_id: rid(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn create_asymmetric_round_trips() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let a = make_entity(&mut client, 1, "A-asym").await;
    let b = make_entity(&mut client, 3, "B-asym").await;

    let (op, body) = round_trip(
        &mut client,
        5,
        RequestBody::RelationCreate(create_request("brain:related_to", a, b)),
    )
    .await;
    assert_eq!(op, Opcode::RelationCreateResp.as_u16());
    let rel_id = match body {
        ResponseBody::RelationCreate(r) => r.relation_id,
        other => panic!("{other:?}"),
    };

    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::RelationGet(RelationGetRequest {
            relation_id: rel_id,
            follow_supersession: false,
        }),
    )
    .await;
    match body {
        ResponseBody::RelationGet(r) => {
            assert_eq!(r.relation.relation_id, rel_id);
            assert_eq!(r.relation.relation_type, "brain:related_to");
            assert_eq!(r.relation.from_entity, a);
            assert_eq!(r.relation.to_entity, b);
            assert_eq!(r.relation.version, 1);
            assert!(!r.relation.tombstoned);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_unknown_relation_type_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "A-unk").await;
    let b = make_entity(&mut client, 3, "B-unk").await;

    let (op, body) = round_trip(
        &mut client,
        5,
        RequestBody::RelationCreate(create_request("user:does_not_exist", a, b)),
    )
    .await;
    assert_eq!(op, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => assert!(e.message.to_lowercase().contains("relation type")),
        other => panic!("{other:?}"),
    }
    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_unknown_endpoint_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let (op, body) = round_trip(
        &mut client,
        1,
        RequestBody::RelationCreate(create_request("brain:related_to", rid(), rid())),
    )
    .await;
    assert_eq!(op, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => assert!(e.message.to_lowercase().contains("entity")),
        other => panic!("{other:?}"),
    }
    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_symmetric_canonicalises() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "A-sym").await;
    let b = make_entity(&mut client, 3, "B-sym").await;

    // Pick whichever is larger for "from" so canonicalisation kicks in.
    let (caller_from, caller_to) = if a > b { (a, b) } else { (b, a) };
    let (canonical_from, canonical_to) = if a < b { (a, b) } else { (b, a) };

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::RelationCreate(create_request("brain:co_authored", caller_from, caller_to)),
    )
    .await;
    let rel_id = match body {
        ResponseBody::RelationCreate(r) => r.relation_id,
        other => panic!("{other:?}"),
    };

    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::RelationGet(RelationGetRequest {
            relation_id: rel_id,
            follow_supersession: false,
        }),
    )
    .await;
    match body {
        ResponseBody::RelationGet(r) => {
            assert_eq!(r.relation.from_entity, canonical_from);
            assert_eq!(r.relation.to_entity, canonical_to);
            assert_eq!(r.relation.flags & 1, 1, "is_symmetric flag set");
        }
        other => panic!("{other:?}"),
    }
    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn create_many_to_one_auto_supersedes() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let priya = make_entity(&mut client, 1, "priya").await;
    let alice = make_entity(&mut client, 3, "alice").await;
    let bob = make_entity(&mut client, 5, "bob").await;

    // First relation: priya reports_to alice.
    let (_, _) = round_trip(
        &mut client,
        7,
        RequestBody::RelationCreate(create_request("brain:reports_to", priya, alice)),
    )
    .await;

    // Second relation: priya reports_to bob — auto-supersedes the
    // first (ManyToOne on `from` side).
    let (op, _) = round_trip(
        &mut client,
        9,
        RequestBody::RelationCreate(create_request("brain:reports_to", priya, bob)),
    )
    .await;
    assert_eq!(op, Opcode::RelationCreateResp.as_u16());

    // List from priya with current_only — exactly one current.
    let (_, body) = round_trip(
        &mut client,
        11,
        RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: priya,
            relation_type_filter: "brain:reports_to".into(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::RelationListFrom(r) => {
            assert_eq!(r.items.len(), 1, "current_only sees one");
            assert_eq!(r.items[0].to_entity, bob);
            assert_eq!(r.items[0].version, 2, "version bumped via auto-supersede");
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn get_missing_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    let (op, body) = round_trip(
        &mut client,
        1,
        RequestBody::RelationGet(RelationGetRequest {
            relation_id: rid(),
            follow_supersession: false,
        }),
    )
    .await;
    assert_eq!(op, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(e) => assert!(e.message.to_lowercase().contains("relation")),
        other => panic!("{other:?}"),
    }
    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn supersede_explicit_returns_new_id_and_version() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "a-sup").await;
    let b = make_entity(&mut client, 3, "b-sup").await;

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::RelationCreate(create_request("brain:related_to", a, b)),
    )
    .await;
    let old_id = match body {
        ResponseBody::RelationCreate(r) => r.relation_id,
        other => panic!("{other:?}"),
    };

    let (op, body) = round_trip(
        &mut client,
        7,
        RequestBody::RelationSupersede(RelationSupersedeRequest {
            old_relation_id: old_id,
            new_relation: create_request("brain:related_to", a, b),
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::RelationSupersedeResp.as_u16());
    match body {
        ResponseBody::RelationSupersede(r) => {
            assert_eq!(r.version, 2);
            assert_ne!(r.new_relation_id, old_id);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tombstone_flips_current_state() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "a-tomb").await;
    let b = make_entity(&mut client, 3, "b-tomb").await;

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::RelationCreate(create_request("brain:related_to", a, b)),
    )
    .await;
    let rel_id = match body {
        ResponseBody::RelationCreate(r) => r.relation_id,
        other => panic!("{other:?}"),
    };

    let (op, _) = round_trip(
        &mut client,
        7,
        RequestBody::RelationTombstone(RelationTombstoneRequest {
            relation_id: rel_id,
            reason: "test".into(),
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::RelationTombstoneResp.as_u16());

    // list_from with default (current_only) excludes.
    let (_, body) = round_trip(
        &mut client,
        9,
        RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: a,
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::RelationListFrom(r) => assert!(r.items.is_empty()),
        other => panic!("{other:?}"),
    }

    // include_tombstoned sees it.
    let (_, body) = round_trip(
        &mut client,
        11,
        RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: a,
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: true,
            include_tombstoned: true,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::RelationListFrom(r) => {
            assert_eq!(r.items.len(), 1);
            assert!(r.items[0].tombstoned);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_to_filters_by_type() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "a-lt").await;
    let b = make_entity(&mut client, 3, "b-lt").await;
    let c = make_entity(&mut client, 5, "c-lt").await;

    round_trip(
        &mut client,
        7,
        RequestBody::RelationCreate(create_request("brain:related_to", a, b)),
    )
    .await;
    round_trip(
        &mut client,
        9,
        RequestBody::RelationCreate(create_request("brain:reports_to", c, b)),
    )
    .await;

    let (_, body) = round_trip(
        &mut client,
        11,
        RequestBody::RelationListTo(RelationListToRequest {
            to_entity: b,
            relation_type_filter: "brain:reports_to".into(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::RelationListTo(r) => {
            assert_eq!(r.items.len(), 1);
            assert_eq!(r.items[0].from_entity, c);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn traverse_one_hop() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "a-tr1").await;
    let b = make_entity(&mut client, 3, "b-tr1").await;

    round_trip(
        &mut client,
        5,
        RequestBody::RelationCreate(create_request("brain:related_to", a, b)),
    )
    .await;

    let (op, body) = round_trip(
        &mut client,
        7,
        RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: a,
            relation_types: vec![],
            direction: 0, // Outgoing
            max_depth: 3,
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::RelationTraverseResp.as_u16());
    match body {
        ResponseBody::RelationTraverse(r) => {
            assert_eq!(r.total_paths, 1);
            assert_eq!(r.paths[0].steps.len(), 1);
            assert_eq!(r.paths[0].steps[0].to, b);
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn traverse_invalid_depth_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;
    let a = make_entity(&mut client, 1, "a-d").await;

    let (op, _) = round_trip(
        &mut client,
        3,
        RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: a,
            relation_types: vec![],
            direction: 0,
            max_depth: 0, // invalid
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::Error.as_u16());
    server.stop().await;
}
