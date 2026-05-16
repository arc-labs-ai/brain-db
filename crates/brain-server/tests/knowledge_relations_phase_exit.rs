//! Phase 18 exit integration test (sub-task 18.9a).
//!
//! Full relation lifecycle end-to-end: create asymmetric chain,
//! create symmetric, traverse, tombstone, re-traverse.

#![cfg(target_os = "linux")]

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::knowledge::statement_req::EvidenceRefWire;
use brain_protocol::knowledge::{
    EntityCreateRequest, RelationCreateRequest, RelationListFromRequest, RelationTombstoneRequest,
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
        client_id: "phase-exit-relation".into(),
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
        other => panic!("{other:?}"),
    }
}

fn rid() -> [u8; 16] {
    *uuid::Uuid::now_v7().as_bytes()
}

fn create_req(relation_type: &str, from: [u8; 16], to: [u8; 16]) -> RelationCreateRequest {
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
// Lifecycle.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn relation_lifecycle_end_to_end() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr).await.expect("connect");
    complete_handshake(&mut client).await;

    // Step 1 — create three Person entities.
    let a = make_entity(&mut client, 1, "lifecycle-a").await;
    let b = make_entity(&mut client, 3, "lifecycle-b").await;
    let c = make_entity(&mut client, 5, "lifecycle-c").await;

    // Step 2 — A → B and B → C via brain:related_to (asymmetric).
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::RelationCreate(create_req("brain:related_to", a, b)),
    )
    .await;
    let ab = match body {
        ResponseBody::RelationCreate(r) => r.relation_id,
        other => panic!("{other:?}"),
    };
    let (_, _) = round_trip(
        &mut client,
        9,
        RequestBody::RelationCreate(create_req("brain:related_to", b, c)),
    )
    .await;

    // Step 3 — Traverse from A depth 2 → 2 paths (A→B, A→B→C).
    let (_, body) = round_trip(
        &mut client,
        11,
        RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: a,
            relation_types: vec!["brain:related_to".into()],
            direction: 0,
            max_depth: 2,
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
            request_id: rid(),
        }),
    )
    .await;
    match body {
        ResponseBody::RelationTraverse(r) => {
            assert_eq!(r.total_paths, 2, "expected 2 paths from A with depth 2");
            let depths: Vec<u32> = r.paths.iter().map(|p| p.steps.last().unwrap().depth).collect();
            assert!(depths.contains(&1));
            assert!(depths.contains(&2));
        }
        other => panic!("{other:?}"),
    }

    // Step 4 — Create symmetric co_authored A ↔ B. list_from from B
    // sees it (dual-indexed).
    let (_, _) = round_trip(
        &mut client,
        13,
        RequestBody::RelationCreate(create_req("brain:co_authored", a, b)),
    )
    .await;
    let (_, body) = round_trip(
        &mut client,
        15,
        RequestBody::RelationListFrom(RelationListFromRequest {
            from_entity: b,
            relation_type_filter: "brain:co_authored".into(),
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
            assert_eq!(r.items.len(), 1, "symmetric is dual-indexed");
        }
        other => panic!("{other:?}"),
    }

    // Step 5 — Tombstone A→B.
    let (op, _) = round_trip(
        &mut client,
        17,
        RequestBody::RelationTombstone(RelationTombstoneRequest {
            relation_id: ab,
            reason: "test tombstone".into(),
            request_id: rid(),
        }),
    )
    .await;
    assert_eq!(op, Opcode::RelationTombstoneResp.as_u16());

    // Step 6 — Re-traverse from A → 0 paths via brain:related_to
    // (A→B is tombstoned; B→C unreachable; default current_only).
    let (_, body) = round_trip(
        &mut client,
        19,
        RequestBody::RelationTraverse(RelationTraverseRequest {
            start_entity: a,
            relation_types: vec!["brain:related_to".into()],
            direction: 0,
            max_depth: 2,
            max_nodes: 100,
            time_at_unix_nanos: 0,
            include_superseded: false,
            request_id: rid(),
        }),
    )
    .await;
    match body {
        ResponseBody::RelationTraverse(r) => {
            assert_eq!(r.total_paths, 0, "tombstoned root edge breaks traversal");
        }
        other => panic!("{other:?}"),
    }

    server.stop().await;
}
