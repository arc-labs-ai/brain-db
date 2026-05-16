//! Schema wire-op smoke (sub-task 19.10a).
//!
//! Drives `SCHEMA_UPLOAD` / `SCHEMA_GET` / `SCHEMA_LIST` /
//! `SCHEMA_VALIDATE` through the full data-plane stack (TCP →
//! frame codec → connection layer → shard executor → brain-ops
//! dispatch → brain-metadata schema_store + brain-protocol schema
//! parser/validator) and asserts:
//!
//! - Each request opcode produces the matching response opcode.
//! - UPLOAD bumps the per-namespace version counter.
//! - LIST returns newest-first.
//! - Parse / validation errors ride in the response body's
//!   `validation_errors` field with `schema_version == 0` (not as
//!   ERROR frames).
//! - Wire-layer errors (empty document, missing namespace) DO
//!   produce ERROR frames.

#![cfg(target_os = "linux")]

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::knowledge::{
    SchemaGetRequest, SchemaListRequest, SchemaUploadRequest, SchemaValidateRequest,
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

// ---------------------------------------------------------------------------
// Frame helpers.
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
        client_id: "schema-tester".into(),
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

fn upload_request(source: &str) -> RequestBody {
    RequestBody::SchemaUpload(SchemaUploadRequest {
        schema_document: source.into(),
        dry_run: false,
        allow_breaking: false,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
    })
}

const ACME_V1: &str = "namespace acme\n\
                       define entity_type Foo { attributes {} }\n";

const ACME_V2: &str = "namespace acme\n\
                       define entity_type Foo { attributes {} }\n\
                       define predicate prefers { kind: Preference object: Value<text> }\n";

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn upload_get_list_smoke() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // UPLOAD v1.
    let (opcode, body) = round_trip(&mut client, 1, upload_request(ACME_V1)).await;
    assert_eq!(opcode, Opcode::SchemaUploadResp.as_u16());
    match body {
        ResponseBody::SchemaUpload(r) => {
            assert_eq!(r.namespace, "acme");
            assert_eq!(r.schema_version, 1);
            assert!(r.validation_errors.is_empty());
        }
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    }

    // GET active (version=0).
    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::SchemaGet(SchemaGetRequest {
            namespace: "acme".into(),
            version: 0,
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::SchemaGetResp.as_u16());
    match body {
        ResponseBody::SchemaGet(r) => {
            assert_eq!(r.namespace, "acme");
            assert_eq!(r.schema_version, 1);
            assert!(r.schema_document.contains("define entity_type Foo"));
            assert_eq!(r.validator_version, 1);
        }
        other => panic!("expected SchemaGetResp, got {other:?}"),
    }

    // LIST.
    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::SchemaList(SchemaListRequest {
            namespace: "acme".into(),
            limit: 0,
            cursor: Vec::new(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::SchemaListResp.as_u16());
    match body {
        ResponseBody::SchemaList(r) => {
            assert_eq!(r.namespace, "acme");
            assert_eq!(r.total, 1);
            assert_eq!(r.items.len(), 1);
            assert_eq!(r.items[0].schema_version, 1);
            assert!(r.is_final);
        }
        other => panic!("expected SchemaListResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn upload_bumps_version_and_list_is_newest_first() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, _) = round_trip(&mut client, 1, upload_request(ACME_V1)).await;
    let (_, body) = round_trip(&mut client, 3, upload_request(ACME_V2)).await;
    match body {
        ResponseBody::SchemaUpload(r) => {
            assert_eq!(r.schema_version, 2);
        }
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    }

    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::SchemaList(SchemaListRequest {
            namespace: "acme".into(),
            limit: 0,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::SchemaList(r) => {
            assert_eq!(r.items.len(), 2);
            assert_eq!(r.items[0].schema_version, 2);
            assert_eq!(r.items[1].schema_version, 1);
        }
        other => panic!("expected SchemaListResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn validate_dry_run_returns_would_be_version() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // Validate on empty active namespace → would_be = 1.
    let (_, body) = round_trip(
        &mut client,
        1,
        RequestBody::SchemaValidate(SchemaValidateRequest {
            schema_document: ACME_V1.into(),
        }),
    )
    .await;
    match body {
        ResponseBody::SchemaValidate(r) => {
            assert_eq!(r.namespace, "acme");
            assert_eq!(r.would_be_version, 1);
            assert!(r.validation_errors.is_empty());
        }
        other => panic!("expected SchemaValidateResp, got {other:?}"),
    }

    // Persist v1, validate again — would_be should now be 2.
    let (_, _) = round_trip(&mut client, 3, upload_request(ACME_V1)).await;
    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::SchemaValidate(SchemaValidateRequest {
            schema_document: ACME_V1.into(),
        }),
    )
    .await;
    match body {
        ResponseBody::SchemaValidate(r) => {
            assert_eq!(r.would_be_version, 2);
        }
        other => panic!("expected SchemaValidateResp, got {other:?}"),
    }

    // LIST still has 1 entry — validate is a dry run.
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::SchemaList(SchemaListRequest {
            namespace: "acme".into(),
            limit: 0,
            cursor: Vec::new(),
        }),
    )
    .await;
    match body {
        ResponseBody::SchemaList(r) => assert_eq!(r.items.len(), 1),
        other => panic!("expected SchemaListResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn parse_error_rides_in_validation_errors_not_error_frame() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(&mut client, 1, upload_request("namespace 123\n")).await;
    assert_eq!(
        opcode,
        Opcode::SchemaUploadResp.as_u16(),
        "parse failure is a structured response, not an ERROR frame"
    );
    match body {
        ResponseBody::SchemaUpload(r) => {
            assert_eq!(r.schema_version, 0);
            assert!(!r.validation_errors.is_empty());
            assert_eq!(r.validation_errors[0].severity, 2);
        }
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn reserved_brain_namespace_validation_error() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, body) = round_trip(
        &mut client,
        1,
        upload_request("namespace brain\ndefine entity_type Foo { attributes {} }\n"),
    )
    .await;
    match body {
        ResponseBody::SchemaUpload(r) => {
            assert_eq!(r.schema_version, 0);
            assert!(r
                .validation_errors
                .iter()
                .any(|e| e.code == "NamespaceInvalidIdentifier"));
        }
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn empty_document_returns_error_frame() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(&mut client, 1, upload_request("")).await;
    assert_eq!(opcode, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(_) => {}
        other => panic!("expected Error frame, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn schema_get_missing_returns_error_frame() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::SchemaGet(SchemaGetRequest {
            namespace: "never_uploaded".into(),
            version: 0,
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::Error.as_u16());
    match body {
        ResponseBody::Error(_) => {}
        other => panic!("expected Error frame, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn system_schema_visible_via_schema_get() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, body) = round_trip(
        &mut client,
        1,
        RequestBody::SchemaGet(SchemaGetRequest {
            namespace: "brain".into(),
            version: 0,
        }),
    )
    .await;
    match body {
        ResponseBody::SchemaGet(r) => {
            assert_eq!(r.namespace, "brain");
            assert_eq!(r.schema_version, 1);
            assert!(r.schema_document.contains("define entity_type Person"));
        }
        other => panic!("expected SchemaGetResp, got {other:?}"),
    }

    server.stop().await;
}
