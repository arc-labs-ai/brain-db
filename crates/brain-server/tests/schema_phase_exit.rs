//! Schema lifecycle exit integration test.
//!
//! Exercises the full schema lifecycle end-to-end over the wire:
//! v1 upload → list → get → v2 upload (adds a predicate) → assert
//! active=2 → assert v1 still readable → validate dry-run → assert
//! list still has 2 entries → system-schema sanity (brain v1).
//!
//! Companion to `schema_wire.rs` (per-op smoke).

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    SchemaGetRequest, SchemaListRequest, SchemaUploadRequest, SchemaValidateRequest,
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

const ACME_V1: &str = "namespace acme\n\
                       define entity_type Foo { attributes {} }\n";

const ACME_V2: &str = "namespace acme\n\
                       define entity_type Foo { attributes {} }\n\
                       define predicate prefers { kind: Preference object: Value<text> }\n";

// ---------------------------------------------------------------------------
// Wire helpers — same as schema_wire.rs.
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
        client_id: "schema-exit".into(),
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

fn upload(text: &str) -> RequestBody {
    RequestBody::SchemaUpload(SchemaUploadRequest {
        schema_document: text.into(),
        dry_run: false,
        allow_breaking: false,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
    })
}

fn get(ns: &str, version: u32) -> RequestBody {
    RequestBody::SchemaGet(SchemaGetRequest {
        namespace: ns.into(),
        version,
    })
}

fn list(ns: &str) -> RequestBody {
    RequestBody::SchemaList(SchemaListRequest {
        namespace: ns.into(),
        limit: 0,
        cursor: Vec::new(),
    })
}

fn validate(text: &str) -> RequestBody {
    RequestBody::SchemaValidate(SchemaValidateRequest {
        schema_document: text.into(),
    })
}

// ---------------------------------------------------------------------------
// Phase-exit lifecycle.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn full_schema_lifecycle_upload_get_list_supersede_validate() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // 1. Upload v1.
    let (_, body) = round_trip(&mut client, 1, upload(ACME_V1)).await;
    let v1_version = match body {
        ResponseBody::SchemaUpload(r) => {
            assert_eq!(r.namespace, "acme");
            assert!(r.validation_errors.is_empty());
            r.schema_version
        }
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    };
    assert_eq!(v1_version, 1);

    // 2. GET active → v1 with verbatim source text.
    let (_, body) = round_trip(&mut client, 3, get("acme", 0)).await;
    match body {
        ResponseBody::SchemaGet(r) => {
            assert_eq!(r.schema_version, 1);
            assert!(r.schema_document.contains("define entity_type Foo"));
            assert!(!r.source_blob.is_empty());
        }
        other => panic!("expected SchemaGetResp, got {other:?}"),
    }

    // 3. Upload v2 (adds a predicate).
    let (_, body) = round_trip(&mut client, 5, upload(ACME_V2)).await;
    let v2_version = match body {
        ResponseBody::SchemaUpload(r) => {
            assert!(r.validation_errors.is_empty());
            r.schema_version
        }
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    };
    assert_eq!(v2_version, 2);

    // 4. Active is now v2.
    let (_, body) = round_trip(&mut client, 7, get("acme", 0)).await;
    match body {
        ResponseBody::SchemaGet(r) => assert_eq!(r.schema_version, 2),
        other => panic!("expected SchemaGetResp, got {other:?}"),
    }

    // 5. v1 still readable by explicit version.
    let (_, body) = round_trip(&mut client, 9, get("acme", 1)).await;
    match body {
        ResponseBody::SchemaGet(r) => {
            assert_eq!(r.schema_version, 1);
            assert!(!r.schema_document.contains("define predicate"));
        }
        other => panic!("expected SchemaGetResp, got {other:?}"),
    }

    // 6. LIST is newest-first.
    let (_, body) = round_trip(&mut client, 11, list("acme")).await;
    match body {
        ResponseBody::SchemaList(r) => {
            assert_eq!(r.items.len(), 2);
            assert_eq!(r.items[0].schema_version, 2);
            assert_eq!(r.items[1].schema_version, 1);
        }
        other => panic!("expected SchemaListResp, got {other:?}"),
    }

    // 7. Validate would-be next version (3) without persisting.
    let (_, body) = round_trip(&mut client, 13, validate(ACME_V2)).await;
    match body {
        ResponseBody::SchemaValidate(r) => {
            assert_eq!(r.would_be_version, 3);
            assert!(r.validation_errors.is_empty());
        }
        other => panic!("expected SchemaValidateResp, got {other:?}"),
    }

    // 8. LIST still has 2 entries — validate didn't persist.
    let (_, body) = round_trip(&mut client, 15, list("acme")).await;
    match body {
        ResponseBody::SchemaList(r) => assert_eq!(r.items.len(), 2),
        other => panic!("expected SchemaListResp, got {other:?}"),
    }

    // 9. System schema sanity — brain v1 is queryable.
    let (_, body) = round_trip(&mut client, 17, get("brain", 0)).await;
    match body {
        ResponseBody::SchemaGet(r) => {
            assert_eq!(r.namespace, "brain");
            assert_eq!(r.schema_version, 1);
            assert!(r.schema_document.contains("define entity_type Person"));
            assert!(r.schema_document.contains("define predicate prefers"));
        }
        other => panic!("expected SchemaGetResp, got {other:?}"),
    }

    server.stop().await;
}
