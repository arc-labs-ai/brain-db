//! Extractor introspection wire-op smoke.
//!
//! Drives `EXTRACTOR_LIST` through the full data-plane stack and
//! asserts it returns the built-in extractors registered by the
//! system-schema bootstrap (`brain.entity_mentions`, `brain.gliner`,
//! `brain.llm_predicate`). Extraction is always-on — there is no
//! runtime enable/disable.

#![cfg(target_os = "linux")]

use std::io::{Read as _, Write as _};
use std::net::TcpStream as StdTcpStream;
use std::time::Duration;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{EncodeRequest, RequestBody};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{ExtractorListRequest, Frame, WaitMode};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use uuid::Uuid;

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

// ---------------------------------------------------------------------------
// Wire helpers — copied from schema_wire.rs.
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

async fn complete_handshake(client: &mut TcpStream, token: &[u8]) {
    let hello = HelloPayload {
        client_id: "extractor-tester".into(),
        supported_versions: vec![brain_protocol::VERSION],
        capabilities: HelloCapabilities {
            streaming: true,
            compression_zstd: false,
            server_push: false,
        },
        client_connection_token: None,
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
        method: AuthMethod::Token,
        credentials: AuthCredentials::Token(token.to_vec()),
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

/// Encode `text`, blocking until the async derivation (extraction) completes
/// (`WaitMode::Derived`), so the resolution-audit rows are durable by the time
/// the call returns.
async fn encode_and_wait(client: &mut TcpStream, stream_id: u32, text: &str) {
    let req = EncodeRequest {
        text: text.into(),
        session_id: 1,
        request_id: *Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: WaitMode::Derived,
        allow_duplicates: false,
    };
    let (opcode, _body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    assert_eq!(opcode, Opcode::EncodeResp.as_u16(), "encode should ack");
}

/// Blocking GET against the admin listener with the test operator secret.
/// Runs under `spawn_blocking` at the call site.
fn http_get_authed(admin_addr: &str, path: &str) -> (u16, String) {
    let mut stream = StdTcpStream::connect_timeout(
        &admin_addr.parse().expect("admin addr"),
        Duration::from_secs(5),
    )
    .expect("connect admin");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let req = format!(
        "GET {path} HTTP/1.1\r\nHost: {admin_addr}\r\n\
         Authorization: Bearer test-admin-token\r\n\
         Connection: close\r\nAccept: */*\r\n\r\n",
    );
    stream.write_all(req.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut raw = Vec::with_capacity(1024);
    stream.read_to_end(&mut raw).unwrap();
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("response delimiter");
    let head = std::str::from_utf8(&raw[..split]).unwrap();
    let status: u16 = head
        .lines()
        .next()
        .unwrap()
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    let body = String::from_utf8_lossy(&raw[split + 4..]).to_string();
    (status, body)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// End-to-end: an ENCODE runs the always-on pattern extractor, which files
/// entity mentions that the apply path resolves — and each resolution now lands
/// a per-mention audit row. Prove the whole derivation→log→query chain by
/// reading the rows back through the admin `GET /v1/audit?by=resolution` route.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn encode_extraction_emits_queryable_resolution_audit() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client, &server.token).await;

    // Proper nouns the model-free pattern tier lifts as entity mentions.
    encode_and_wait(&mut client, 1, "Alice Johnson works at Acme Corporation").await;

    let admin_addr = server.admin_addr.to_string();
    let (code, body) = tokio::task::spawn_blocking(move || {
        http_get_authed(&admin_addr, "/v1/audit?by=resolution")
    })
    .await
    .unwrap();

    assert_eq!(code, 200, "body:\n{body}");
    assert!(
        body.contains("\"kind\":\"resolution\""),
        "resolution rows expected; body:\n{body}",
    );
    // At least one proper-noun mention resolved into a durable, queryable
    // derivation row.
    assert!(
        body.contains("\"resolved_entity\"") || body.contains("Acme") || body.contains("Alice"),
        "a resolved mention should appear; body:\n{body}",
    );

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extractor_list_returns_seeded_builtins() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client, &server.token).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorList(ExtractorListRequest {}),
    )
    .await;
    assert_eq!(opcode, Opcode::ExtractorListResp.as_u16());
    match body {
        ResponseBody::ExtractorList(r) => {
            assert!(r.is_final);
            // The system-schema bootstrap seeds the three extraction tiers.
            assert_eq!(r.items.len(), 3);
            assert_eq!(r.total, 3);
            let names: Vec<&str> = r.items.iter().map(|i| i.name.as_str()).collect();
            assert!(names.contains(&"entity_mentions"));
            assert!(names.contains(&"gliner"));
            assert!(names.contains(&"llm_predicate"));
            for item in &r.items {
                assert_eq!(item.namespace, "brain");
            }
        }
        other => panic!("expected ExtractorListResp, got {other:?}"),
    }

    server.stop().await;
}
