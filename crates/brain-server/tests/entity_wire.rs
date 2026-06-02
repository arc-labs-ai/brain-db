//! Entity wire-op smoke.
//!
//! Drives `ENTITY_CREATE` / `ENTITY_GET` / `ENTITY_UPDATE` / `ENTITY_RENAME`
//! through the full data-plane stack (TCP → frame codec → connection
//! layer → shard executor → brain-ops dispatch → brain-metadata
//! entity_ops) and asserts:
//!
//! - Each request opcode produces the matching response opcode.
//! - Lifecycle persists: create → get → update → get → rename → get.
//! - Negative paths return ERROR with the expected category.
//!
//! Mirrors the e2e.rs wire-smoke pattern (Linux-only via zigbuild).

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    EntityCreateRequest, EntityGetRequest, EntityRenameRequest, EntityUpdateRequest,
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
/// `Person` entity-type id seeded by `MetadataDb::open`.
const PERSON_TYPE_ID: u32 = 1;

// ---------------------------------------------------------------------------
// Frame helpers
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
        client_id: "wire-tester".into(),
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

/// Send one request frame, read one response. Returns the decoded
/// response body alongside the wire opcode for sanity checks.
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn entity_create_get_update_rename_lifecycle() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // CREATE
    let (create_opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: PERSON_TYPE_ID,
            canonical_name: "Alice".into(),
            aliases: vec![],
            attributes_blob: Vec::new(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(
        create_opcode,
        Opcode::EntityCreateResp.as_u16(),
        "create resp opcode"
    );
    let entity_id = match body {
        ResponseBody::EntityCreate(r) => {
            assert_ne!(r.entity_id, [0u8; 16], "non-zero EntityId");
            r.entity_id
        }
        other => panic!("expected EntityCreateResp, got {other:?}"),
    };

    // GET
    let (get_opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::EntityGet(EntityGetRequest { entity_id }),
    )
    .await;
    assert_eq!(get_opcode, Opcode::EntityGetResp.as_u16());
    match body {
        ResponseBody::EntityGet(r) => {
            assert_eq!(r.entity.entity_id, entity_id);
            assert_eq!(r.entity.canonical_name, "Alice");
            assert_eq!(r.entity.normalized_name, "alice");
            assert_eq!(r.entity.entity_type_id, PERSON_TYPE_ID);
            assert!(r.entity.aliases.is_empty());
            assert_eq!(r.entity.embedding_version, 0);
        }
        other => panic!("expected EntityGetResp, got {other:?}"),
    }

    // UPDATE (attribute change, same canonical_name)
    let (update_opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::EntityUpdate(EntityUpdateRequest {
            entity_id,
            canonical_name: "Alice".into(),
            aliases: vec!["A.".into()],
            attributes_blob: b"bio=astronaut".to_vec(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(update_opcode, Opcode::EntityUpdateResp.as_u16());
    match body {
        ResponseBody::EntityUpdate(r) => {
            assert_eq!(r.entity.canonical_name, "Alice");
            assert_eq!(r.entity.aliases, vec!["A.".to_string()]);
            assert_eq!(r.entity.attributes_blob, b"bio=astronaut");
            // canonical_name unchanged → embedding_version unchanged.
            assert_eq!(r.entity.embedding_version, 0);
        }
        other => panic!("expected EntityUpdateResp, got {other:?}"),
    }

    // RENAME (move_to_alias=true)
    let (rename_opcode, body) = round_trip(
        &mut client,
        7,
        RequestBody::EntityRename(EntityRenameRequest {
            entity_id,
            new_canonical_name: "Alice Cooper".into(),
            move_to_alias: true,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(rename_opcode, Opcode::EntityRenameResp.as_u16());
    match body {
        ResponseBody::EntityRename(r) => {
            assert_eq!(r.entity.canonical_name, "Alice Cooper");
            assert_eq!(r.entity.normalized_name, "alice cooper");
            assert!(
                r.entity.aliases.iter().any(|a| a == "Alice"),
                "old name trailed as alias: {:?}",
                r.entity.aliases
            );
            assert!(r.entity.embedding_version >= 1, "version bumped on rename");
        }
        other => panic!("expected EntityRenameResp, got {other:?}"),
    }

    // GET again to confirm post-rename persistence.
    let (_, body) = round_trip(
        &mut client,
        9,
        RequestBody::EntityGet(EntityGetRequest { entity_id }),
    )
    .await;
    match body {
        ResponseBody::EntityGet(r) => {
            assert_eq!(r.entity.canonical_name, "Alice Cooper");
        }
        other => panic!("expected EntityGetResp after rename, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn entity_get_missing_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let bogus_id = *uuid::Uuid::now_v7().as_bytes();
    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::EntityGet(EntityGetRequest {
            entity_id: bogus_id,
        }),
    )
    .await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "missing entity surfaces ERROR"
    );
    match body {
        ResponseBody::Error(e) => {
            assert!(
                e.message.to_lowercase().contains("entity")
                    || e.message.to_lowercase().contains("not found"),
                "error message references entity: {:?}",
                e.message
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn entity_create_unknown_type_returns_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::EntityCreate(EntityCreateRequest {
            entity_type_id: 999, // unregistered
            canonical_name: "Should fail".into(),
            aliases: vec![],
            attributes_blob: Vec::new(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "unknown type surfaces ERROR"
    );
    match body {
        ResponseBody::Error(e) => {
            assert!(
                e.message.to_lowercase().contains("entity_type")
                    || e.message.to_lowercase().contains("type"),
                "error mentions entity type: {:?}",
                e.message
            );
        }
        other => panic!("expected Error, got {other:?}"),
    }

    server.stop().await;
}
