//! Entity merge / unmerge / resolve / list / tombstone wire integration
//! tests.
//!
//! Drives the five new opcodes (and re-tests the existing four with
//! event emission) through the full data-plane stack.
//!
//! Mirrors `entity_wire.rs`'s pattern (Linux-only via zigbuild).

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    EntityCreateRequest, EntityGetRequest, EntityListRequest, EntityMergeRequest,
    EntityResolveRequest, EntityTombstoneRequest, EntityUnmergeRequest, ResolutionOutcomeWire,
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
// Wire helpers (shared shape with entity_wire.rs).
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
        client_id: "merge-tester".into(),
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

async fn create_person(
    client: &mut TcpStream,
    stream_id: u32,
    name: &str,
    aliases: Vec<String>,
) -> [u8; 16] {
    let (_, body) = round_trip(
        client,
        stream_id,
        RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: PERSON_TYPE_ID,
            canonical_name: name.into(),
            aliases,
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn merge_and_unmerge_round_trip() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;
    let alyss = create_person(&mut client, 3, "Alyss", vec!["AL".into()]).await;

    // MERGE Alyss → Alice.
    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::EntityMerge(EntityMergeRequest {
            survivor: alice,
            merged: alyss,
            confidence: 0.92,
            reason: "duplicate found".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::EntityMergeResp.as_u16());
    let merge_audit_id = match body {
        ResponseBody::EntityMerge(r) => {
            assert_ne!(r.audit_id, [0u8; 16]);
            assert_eq!(r.grace_period_seconds, 7 * 24 * 60 * 60);
            r.audit_id
        }
        other => panic!("expected EntityMerge resp, got {other:?}"),
    };
    let _ = merge_audit_id; // not asserted further

    // GET Alyss: now merged.
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::EntityGet(EntityGetRequest { entity_id: alyss }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert_ne!(r.entity.merged_into, [0u8; 16], "merged_into populated");
            assert_eq!(r.entity.merged_into, alice);
            assert_ne!(r.entity.flags & 2, 0, "MERGED flag set");
        }
        other => panic!("expected EntityGet, got {other:?}"),
    }

    // GET Alice: gained Alyss's name + alias.
    let (_, body) = round_trip(
        &mut client,
        9,
        RequestBody::EntityGet(EntityGetRequest { entity_id: alice }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert!(
                r.entity.aliases.contains(&"Alyss".into()),
                "Alice gained Alyss's canonical_name as alias: {:?}",
                r.entity.aliases
            );
            assert!(
                r.entity.aliases.contains(&"AL".into()),
                "Alice gained AL alias: {:?}",
                r.entity.aliases
            );
        }
        other => panic!("expected EntityGet, got {other:?}"),
    }

    // UNMERGE Alyss back out.
    let (opcode, body) = round_trip(
        &mut client,
        11,
        RequestBody::EntityUnmerge(EntityUnmergeRequest {
            merged_entity: alyss,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::EntityUnmergeResp.as_u16());
    match body {
        ResponseBody::EntityUnmerge(r) => assert_eq!(r.restored_entity_id, alyss),
        other => panic!("expected EntityUnmerge, got {other:?}"),
    }

    // GET Alyss: independent again.
    let (_, body) = round_trip(
        &mut client,
        13,
        RequestBody::EntityGet(EntityGetRequest { entity_id: alyss }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert_eq!(r.entity.merged_into, [0u8; 16], "merged_into cleared");
            assert_eq!(r.entity.flags & 2, 0, "MERGED flag cleared");
        }
        other => panic!("expected EntityGet, got {other:?}"),
    }

    // GET Alice: aliases stripped of Alyss/AL.
    let (_, body) = round_trip(
        &mut client,
        15,
        RequestBody::EntityGet(EntityGetRequest { entity_id: alice }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert!(
                !r.entity.aliases.iter().any(|a| a == "Alyss"),
                "Alyss removed from Alice's aliases on unmerge: {:?}",
                r.entity.aliases
            );
            assert!(
                !r.entity.aliases.iter().any(|a| a == "AL"),
                "AL removed: {:?}",
                r.entity.aliases
            );
        }
        other => panic!("expected EntityGet, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn merge_self_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;
    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::EntityMerge(EntityMergeRequest {
            survivor: alice,
            merged: alice,
            confidence: 0.9,
            reason: "self-merge".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(_) => {}
        other => panic!("expected Error, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn merge_low_confidence_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;
    let bob = create_person(&mut client, 3, "Bob", vec![]).await;
    let (opcode, _) = round_trip(
        &mut client,
        5,
        RequestBody::EntityMerge(EntityMergeRequest {
            survivor: alice,
            merged: bob,
            confidence: 0.5, // below threshold
            reason: "low-confidence test".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::Error.as_u16());

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn tombstone_then_get_shows_flag() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;

    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::EntityTombstone(EntityTombstoneRequest {
            entity_id: alice,
            reason: "obsolete".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::EntityTombstoneResp.as_u16());
    match body {
        ResponseBody::EntityTombstone(r) => assert!(r.tombstoned_at_unix_nanos > 0),
        other => panic!("expected EntityTombstone, got {other:?}"),
    }

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::EntityGet(EntityGetRequest { entity_id: alice }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert_ne!(r.entity.flags & 1, 0, "TOMBSTONED flag set");
        }
        other => panic!("expected EntityGet, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn resolve_exact_match_via_canonical_name() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;

    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::EntityResolve(EntityResolveRequest {
            candidate_name: "Alice".into(),
            context: String::new(),
            entity_type_hint: PERSON_TYPE_ID,
            allow_create: false,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::EntityResolveResp.as_u16());
    match body {
        ResponseBody::EntityResolve(r) => {
            assert_eq!(r.outcome, ResolutionOutcomeWire::Resolved);
            assert_eq!(r.tier, 1);
            assert_eq!(r.resolved_entity, alice);
            assert!(r.confidence >= 0.99);
        }
        other => panic!("expected EntityResolve, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn resolve_unknown_returns_not_found() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    create_person(&mut client, 1, "Alice", vec![]).await;

    let (_, body) = round_trip(
        &mut client,
        3,
        RequestBody::EntityResolve(EntityResolveRequest {
            candidate_name: "Zelda".into(),
            context: String::new(),
            entity_type_hint: PERSON_TYPE_ID,
            allow_create: false,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityResolve(r) => {
            assert_eq!(r.outcome, ResolutionOutcomeWire::NotFound);
        }
        other => panic!("expected EntityResolve, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_returns_created_entities() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;
    let bob = create_person(&mut client, 3, "Bob", vec![]).await;
    let carol = create_person(&mut client, 5, "Carol", vec![]).await;

    let (opcode, body) = round_trip(
        &mut client,
        7,
        RequestBody::EntityList(EntityListRequest {
            entity_type_id: PERSON_TYPE_ID,
            name_prefix: String::new(),
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::EntityListResp.as_u16());
    match body {
        ResponseBody::EntityList(frame) => {
            assert!(frame.is_final, "single-frame snapshot in 16.7");
            assert!(frame.next_cursor.is_empty());
            assert_eq!(frame.items.len(), 3);
            assert_eq!(frame.cumulative_count, 3);
            let ids: std::collections::HashSet<[u8; 16]> =
                frame.items.iter().map(|i| i.entity.entity_id).collect();
            assert!(ids.contains(&alice));
            assert!(ids.contains(&bob));
            assert!(ids.contains(&carol));
        }
        other => panic!("expected EntityList, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_with_name_prefix_filters() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let alice = create_person(&mut client, 1, "Alice", vec![]).await;
    let _bob = create_person(&mut client, 3, "Bob", vec![]).await;

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::EntityList(EntityListRequest {
            entity_type_id: PERSON_TYPE_ID,
            name_prefix: "ali".into(), // normalized prefix
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityList(frame) => {
            assert_eq!(frame.items.len(), 1, "only Alice matches");
            assert_eq!(frame.items[0].entity.entity_id, alice);
        }
        other => panic!("expected EntityList, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn list_cursor_rejected_in_v1() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    create_person(&mut client, 1, "Alice", vec![]).await;

    let (opcode, _) = round_trip(
        &mut client,
        3,
        RequestBody::EntityList(EntityListRequest {
            entity_type_id: PERSON_TYPE_ID,
            name_prefix: String::new(),
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: 100,
            cursor: vec![0xAB, 0xCD],
        }),
    )
    .await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "cursor pagination is deferred to phase 23"
    );

    server.stop().await;
}
