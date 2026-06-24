//! Extractor governance wire-op smoke.
//!
//! Drives `EXTRACTOR_LIST` / `EXTRACTOR_DISABLE` / `EXTRACTOR_ENABLE`
//! through the full data-plane stack and asserts:
//!
//! - LIST returns the built-in extractors registered by the system
//!   schema bootstrap (`brain.entity_mentions`, `brain.gliner`,
//!   `brain.llm_predicate`).
//! - `include_disabled = false` filters out disabled rows.
//! - DISABLE flips a row from enabled → disabled, returns
//!   `previously_enabled = true`.
//! - ENABLE flips it back, returns `previously_disabled = true`.
//! - Unknown extractor_id → ERROR with NotFound category.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{ExtractorDisableRequest, ExtractorEnableRequest, ExtractorListRequest};
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

async fn complete_handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "extractor-tester".into(),
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

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn extractor_list_returns_seeded_builtins() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
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
                assert!(item.enabled, "built-ins enabled by default");
            }
        }
        other => panic!("expected ExtractorListResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extractor_disable_then_list_excludes_disabled() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // Resolve the entity_mentions id via LIST.
    let (_, body) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
    )
    .await;
    let entity_mentions_id = match body {
        ResponseBody::ExtractorList(r) => {
            r.items
                .iter()
                .find(|i| i.name == "entity_mentions")
                .expect("entity_mentions exists")
                .extractor_id
        }
        _ => unreachable!(),
    };

    // DISABLE it.
    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: entity_mentions_id,
            reason: "test disable".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::ExtractorDisableResp.as_u16());
    match body {
        ResponseBody::ExtractorDisable(r) => {
            assert!(r.previously_enabled);
        }
        other => panic!("expected ExtractorDisableResp, got {other:?}"),
    }

    // LIST with include_disabled=false: the two still-enabled built-ins,
    // entity_mentions filtered out.
    let (_, body) = round_trip(
        &mut client,
        5,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: false,
        }),
    )
    .await;
    match body {
        ResponseBody::ExtractorList(r) => {
            assert_eq!(r.items.len(), 2);
            assert!(
                !r.items.iter().any(|i| i.name == "entity_mentions"),
                "disabled extractor must be filtered out"
            );
        }
        _ => unreachable!(),
    }

    // LIST with include_disabled=true: all three, entity_mentions disabled.
    let (_, body) = round_trip(
        &mut client,
        7,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
    )
    .await;
    match body {
        ResponseBody::ExtractorList(r) => {
            assert_eq!(r.items.len(), 3);
            let entity_mentions = r
                .items
                .iter()
                .find(|i| i.name == "entity_mentions")
                .unwrap();
            assert!(!entity_mentions.enabled);
        }
        _ => unreachable!(),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extractor_enable_after_disable_returns_previously_disabled() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, body) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
    )
    .await;
    let id = match body {
        ResponseBody::ExtractorList(r) => {
            r.items
                .iter()
                .find(|i| i.name == "gliner")
                .unwrap()
                .extractor_id
        }
        _ => unreachable!(),
    };

    // Disable.
    let (_, _) = round_trip(
        &mut client,
        3,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: id,
            reason: "test".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;

    // Enable.
    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::ExtractorEnable(ExtractorEnableRequest {
            extractor_id: id,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::ExtractorEnableResp.as_u16());
    match body {
        ResponseBody::ExtractorEnable(r) => {
            assert!(r.previously_disabled);
        }
        other => panic!("expected ExtractorEnableResp, got {other:?}"),
    }

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extractor_disable_unknown_id_returns_error_frame() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, body) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: 99999,
            reason: "test".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
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
async fn extractor_disable_zero_id_returns_error_frame() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (opcode, _) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: 0,
            reason: "x".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::Error.as_u16());

    server.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn extractor_disable_oversized_reason_returns_error_frame() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let big = "x".repeat(4097);
    let (opcode, _) = round_trip(
        &mut client,
        1,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: 1,
            reason: big,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::Error.as_u16());

    server.stop().await;
}
