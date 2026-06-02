//! Entity lifecycle exit integration test.
//!
//! Exercises the full entity lifecycle end-to-end over the wire:
//! create → get → update → rename → merge → unmerge → rename →
//! list → tombstone → list (include vs exclude tombstoned).
//!
//! Companion to:
//!
//! - `entity_wire.rs`: individual op wire smoke.
//! - `entity_merge_wire.rs`: merge / unmerge /
//!   resolve / list / tombstone wire smoke + error paths.
//!
//! This test focuses on **lifecycle ordering** — that the operations
//! compose correctly across a single entity's history, in the order a
//! real operator would issue them.

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
    EntityRenameRequest, EntityTombstoneRequest, EntityUnmergeRequest, EntityUpdateRequest,
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
// Wire helpers (mirror entity_*_wire.rs).
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
        client_id: "phase-exit-tester".into(),
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
// The phase-exit lifecycle test.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn full_entity_lifecycle_create_merge_unmerge_rename_list_tombstone() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let mut stream = 1u32;
    let next = |s: &mut u32| {
        let cur = *s;
        *s += 2;
        cur
    };

    // ---- 1. CREATE Alice and Alyss (with alias) ----
    let alice = create_person(&mut client, next(&mut stream), "Alice", vec![]).await;
    let alyss = create_person(&mut client, next(&mut stream), "Alyss", vec!["AL".into()]).await;

    // ---- 2. UPDATE Alice — add an alias attribute slot (replace mode) ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityUpdate(EntityUpdateRequest {
            entity_id: alice,
            canonical_name: "Alice".into(),
            aliases: vec!["A.".into()],
            attributes_blob: Vec::new(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityUpdate(r) => {
            assert!(r.entity.aliases.contains(&"A.".into()));
        }
        other => panic!("update expected, got {other:?}"),
    }

    // ---- 3. RENAME Alice → "Alice Cooper" (old name moves to alias) ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityRename(EntityRenameRequest {
            entity_id: alice,
            new_canonical_name: "Alice Cooper".into(),
            move_to_alias: true,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityRename(r) => {
            assert_eq!(r.entity.canonical_name, "Alice Cooper");
            assert!(r.entity.aliases.iter().any(|a| a == "Alice"));
            assert!(r.entity.embedding_version >= 1);
        }
        other => panic!("rename expected, got {other:?}"),
    }

    // ---- 4. MERGE Alyss → Alice ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityMerge(EntityMergeRequest {
            survivor: alice,
            merged: alyss,
            confidence: 0.93,
            reason: "duplicate (lifecycle test)".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    let merge_audit = match body {
        ResponseBody::EntityMerge(r) => r.audit_id,
        other => panic!("merge expected, got {other:?}"),
    };
    assert_ne!(merge_audit, [0u8; 16]);

    // ---- 5. GET Alice — should now hold Alyss's name + alias ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityGet(EntityGetRequest { entity_id: alice }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert!(r.entity.aliases.iter().any(|a| a == "Alyss"));
            assert!(r.entity.aliases.iter().any(|a| a == "AL"));
        }
        other => panic!("get expected, got {other:?}"),
    }

    // ---- 6. UNMERGE Alyss back out ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityUnmerge(EntityUnmergeRequest {
            merged_entity: alyss,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityUnmerge(r) => assert_eq!(r.restored_entity_id, alyss),
        other => panic!("unmerge expected, got {other:?}"),
    }

    // ---- 7. RENAME Alice again — verify post-unmerge state allows rename ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityRename(EntityRenameRequest {
            entity_id: alice,
            new_canonical_name: "Alice C.".into(),
            move_to_alias: true,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityRename(r) => {
            assert_eq!(r.entity.canonical_name, "Alice C.");
            assert!(r.entity.aliases.iter().any(|a| a == "Alice Cooper"));
            // Alyss's contribution was stripped on unmerge.
            assert!(!r.entity.aliases.iter().any(|a| a == "Alyss"));
            assert!(!r.entity.aliases.iter().any(|a| a == "AL"));
        }
        other => panic!("rename expected, got {other:?}"),
    }

    // ---- 8. LIST — both entities reachable, neither tombstoned ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
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
    match body {
        ResponseBody::EntityList(frame) => {
            assert_eq!(frame.items.len(), 2);
            let ids: std::collections::HashSet<[u8; 16]> =
                frame.items.iter().map(|i| i.entity.entity_id).collect();
            assert!(ids.contains(&alice));
            assert!(ids.contains(&alyss));
        }
        other => panic!("list expected, got {other:?}"),
    }

    // ---- 9. TOMBSTONE Alyss ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityTombstone(EntityTombstoneRequest {
            entity_id: alyss,
            reason: "phase-exit cleanup".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityTombstone(r) => assert!(r.tombstoned_at_unix_nanos > 0),
        other => panic!("tombstone expected, got {other:?}"),
    }

    // ---- 10. LIST — Alyss absent unless include_tombstoned ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
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
    match body {
        ResponseBody::EntityList(frame) => {
            let ids: std::collections::HashSet<[u8; 16]> =
                frame.items.iter().map(|i| i.entity.entity_id).collect();
            assert!(ids.contains(&alice));
            assert!(
                !ids.contains(&alyss),
                "Alyss hidden by include_tombstoned=false"
            );
        }
        other => panic!("list expected, got {other:?}"),
    }

    // ---- 11. LIST with include_tombstoned=true — Alyss visible with flag ----
    let (_, body) = round_trip(
        &mut client,
        next(&mut stream),
        RequestBody::EntityList(EntityListRequest {
            entity_type_id: PERSON_TYPE_ID,
            name_prefix: String::new(),
            mention_count_min: 0,
            include_tombstoned: true,
            include_merged: false,
            limit: 100,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::EntityList(frame) => {
            let alyss_item = frame
                .items
                .iter()
                .find(|i| i.entity.entity_id == alyss)
                .expect("Alyss visible with include_tombstoned=true");
            assert_ne!(
                alyss_item.entity.flags & 1,
                0,
                "TOMBSTONED bit set on flags"
            );
        }
        other => panic!("list expected, got {other:?}"),
    }

    server.stop().await;
}
