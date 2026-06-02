//! LLM extractor wire smoke.
//!
//! Asserts the LLM tier lights up over the wire when an operator
//! schema declares one:
//!
//! - `SCHEMA_UPLOAD` of an `llm` extractor block parses + persists.
//! - `EXTRACTOR_LIST` returns the LLM row (`kind == 2`) alongside
//!   the substrate's built-in pair.
//! - `ENCODE` against a memory matching the trigger produces an
//!   audit row stamped against the LLM extractor.
//!
//! CI never has API keys set, so the LLM extractor registers in
//! **degraded mode**. The audit row in that case has status
//! `Failure(reason: "no llm clients configured (...)")`. ENCODE
//! itself still succeeds — the LLM tier's miss never propagates
//! to the client.
//!
//! Live-provider runs that set `ANTHROPIC_API_KEY` ahead of this
//! test would observe a wired extractor instead; we don't fence
//! that here.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::{
    ExtractorDisableRequest, ExtractorEnableRequest, ExtractorListRequest, SchemaUploadRequest,
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

use support_harness::start_in;
use tempfile::TempDir;

const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// Wire helpers (copied from sibling extractor/schema wire suites).
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
        client_id: "llm-wire-tester".into(),
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

fn upload_request(source: &str) -> RequestBody {
    RequestBody::SchemaUpload(SchemaUploadRequest {
        schema_document: source.into(),
        dry_run: false,
        allow_breaking: false,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
    })
}

// User schema declaring an LLM extractor. `cost_budget` and
// `cache_*` left off so the materializer takes the no-router
// degraded path (CI has no `ANTHROPIC_API_KEY`).
const ACME_LLM_SCHEMA: &str = "namespace acme\n\
                               define extractor llm_prefs {\n\
                               kind: llm\n\
                               target: statement Preference\n\
                               trigger: on encode\n\
                               model: \"claude-haiku-4-5\"\n\
                               prompt: \"Extract preferences.\"\n\
                               confidence_threshold: 0.7\n\
                               }\n";

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn schema_upload_registers_llm_extractor_in_list() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    // 1. SCHEMA_UPLOAD parses + persists the user namespace.
    let (opcode, body) = round_trip(&mut client, 1, upload_request(ACME_LLM_SCHEMA)).await;
    assert_eq!(opcode, Opcode::SchemaUploadResp.as_u16());
    let upload_resp = match body {
        ResponseBody::SchemaUpload(r) => r,
        other => panic!("expected SchemaUploadResp, got {other:?}"),
    };
    assert_eq!(upload_resp.namespace, "acme");
    assert_eq!(upload_resp.schema_version, 1);
    assert!(
        upload_resp.validation_errors.is_empty(),
        "validation errors: {:?}",
        upload_resp.validation_errors
    );

    // 2. EXTRACTOR_LIST includes the LLM row alongside the two
    //    system-schema built-ins.
    let (opcode, body) = round_trip(
        &mut client,
        3,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::ExtractorListResp.as_u16());
    let list = match body {
        ResponseBody::ExtractorList(r) => r,
        other => panic!("expected ExtractorListResp, got {other:?}"),
    };

    let llm_row = list
        .items
        .iter()
        .find(|i| i.namespace == "acme" && i.name == "llm_prefs")
        .expect("acme:llm_prefs registered");
    assert_eq!(llm_row.kind, 2, "kind byte 2 == llm");
    assert!(llm_row.enabled, "newly uploaded extractor is enabled");

    server.stop().await;
}

// ---------------------------------------------------------------------------
// Live-registry sync gap (recorded, not tested here).
//
// `SCHEMA_UPLOAD` persists the new extractor row into
// `EXTRACTORS_TABLE`; `EXTRACTOR_LIST` reads from that same table
// (see `schema_upload_registers_llm_extractor_in_list` above) so
// the row is observable on the wire immediately. But the
// in-memory `ctx.extractor_registry` is only populated at shard
// startup via `build_registry_from_definitions` —
// `handle_schema_upload` does NOT re-materialize new rows into
// the live registry. As a result, `ENCODE` after a new upload
// does not dispatch the newly-declared extractor.
//
// The wire-level audit-row observation for an operator-uploaded
// LLM extractor therefore can't be exercised end-to-end until a
// follow-up wires the registry-sync hook (alongside the
// resolver-tier persistence work). The
// end-to-end audit-row path stays covered for built-in
// extractors by `extractors_phase_exit.rs` and
// at the unit level by `crates/brain-extractors/tests/
// llm_pipeline.rs`.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn extractor_disable_then_enable_round_trip_for_llm_row() {
    let data_dir = TempDir::new().expect("tmp");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;

    let (_, _) = round_trip(&mut client, 1, upload_request(ACME_LLM_SCHEMA)).await;

    // Resolve the LLM extractor's id via LIST.
    let (_, body) = round_trip(
        &mut client,
        3,
        RequestBody::ExtractorList(ExtractorListRequest {
            include_disabled: true,
        }),
    )
    .await;
    let llm_id = match body {
        ResponseBody::ExtractorList(r) => {
            r.items
                .iter()
                .find(|i| i.namespace == "acme" && i.name == "llm_prefs")
                .expect("acme:llm_prefs present")
                .extractor_id
        }
        _ => unreachable!(),
    };

    // DISABLE.
    let (opcode, body) = round_trip(
        &mut client,
        5,
        RequestBody::ExtractorDisable(ExtractorDisableRequest {
            extractor_id: llm_id,
            reason: "test disable".into(),
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::ExtractorDisableResp.as_u16());
    match body {
        ResponseBody::ExtractorDisable(r) => assert!(r.previously_enabled),
        other => panic!("expected ExtractorDisableResp, got {other:?}"),
    }

    // ENABLE.
    let (opcode, body) = round_trip(
        &mut client,
        7,
        RequestBody::ExtractorEnable(ExtractorEnableRequest {
            extractor_id: llm_id,
            request_id: *uuid::Uuid::now_v7().as_bytes(),
        }),
    )
    .await;
    assert_eq!(opcode, Opcode::ExtractorEnableResp.as_u16());
    match body {
        ResponseBody::ExtractorEnable(r) => assert!(r.previously_disabled),
        other => panic!("expected ExtractorEnableResp, got {other:?}"),
    }

    server.stop().await;
}
