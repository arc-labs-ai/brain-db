//! Per-request effective-identity (`act_as`) isolation on a single connection.
//!
//! The gateway/edge deployment model authenticates ONE trusted service
//! principal to Brain and then runs every downstream tenant's op on that one
//! connection by attaching a per-request `act_as` selector — no per-tenant
//! connection, no forwarded raw client key. This test proves the two
//! properties that make that model safe:
//!
//!   1. **Effective-identity isolation.** Two tenants driven through the SAME
//!      connection, distinguished only by the per-request `act_as`, never see
//!      each other's memories. Tenant A's RECALL (act_as = A) returns only A's
//!      rows; tenant B's (act_as = B) only B's.
//!
//!   2. **Privilege gate before switch (R1/R2).** The switch is hard-gated:
//!      a principal WITHOUT the `ACT_AS` grant that supplies an `act_as` is
//!      rejected with `ActAsDenied` (never silently downgraded to its own
//!      identity), and a principal WITH the grant that names a namespace
//!      outside its `may_act` allowlist is likewise rejected.
//!
//!   3. **SUBSCRIBE honors the same `act_as` contract.** Unlike every other
//!      data-plane op, SUBSCRIBE bypasses the normal `run_op_dispatch` /
//!      `act_as_of` path structurally (it mutates the connection-layer
//!      `SubscriptionRegistry`, not `brain_ops`). It still runs the exact
//!      same R1/R2 checks and routes to the effective space, so a
//!      shared-pool caller can scope a subscription to a different space
//!      it's permitted to `act_as` for — see the `subscribe_act_as_*` tests
//!      below.
//!
//! A single shard (`start(1)`) collocates every identity on shard 0, so what's
//! under test is the logical effective-identity scoping, not incidental
//! physical shard separation. The stub dispatcher embeds zero vectors, so
//! assertions are on result-set membership only, never score ordering.

#![cfg(target_os = "linux")]

use std::time::Duration;

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, RecallRequest, RequestBody, SubscribeRequest, SubscriptionFilter,
};
use brain_protocol::envelope::response::{ErrorCodeWire, ResponseBody};
use brain_protocol::{ActAs, EventType, Frame};
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
// Wire helpers (mirrors space_isolation.rs)
// ---------------------------------------------------------------------------

async fn read_one_frame<S>(stream: &mut S) -> Frame
where
    S: tokio::io::AsyncRead + Unpin,
{
    let mut header = [0u8; brain_protocol::HEADER_SIZE];
    stream.read_exact(&mut header).await.expect("header read");
    let payload_len = u32::from_be_bytes([0, header[16], header[17], header[18]]) as usize;
    let mut buf = Vec::with_capacity(brain_protocol::HEADER_SIZE + payload_len);
    buf.extend_from_slice(&header);
    if payload_len > 0 {
        buf.resize(brain_protocol::HEADER_SIZE + payload_len, 0);
        stream
            .read_exact(&mut buf[brain_protocol::HEADER_SIZE..])
            .await
            .expect("payload read");
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

/// Read one frame within `within`; `None` on timeout. SUBSCRIBE_EVENT
/// frames are server-pushed with no synchronous ack, so subscribe tests
/// can't use `round_trip` — they poll the raw stream instead.
async fn read_frame_within(client: &mut TcpStream, within: Duration) -> Option<Frame> {
    tokio::time::timeout(within, read_one_frame(client))
        .await
        .ok()
}

async fn round_trip(
    client: &mut TcpStream,
    stream_id: u32,
    req: RequestBody,
) -> (u16, ResponseBody) {
    let opcode = req.opcode().as_u16();
    send_frame(
        client,
        Frame::new(opcode, FLAG_EOS, stream_id, req.encode()),
    )
    .await;
    let resp = read_one_frame(client).await;
    let resp_opcode = resp.header.opcode_u16();
    let body = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    (resp_opcode, body)
}

async fn handshake_as(client: &mut TcpStream, token: &[u8]) {
    let hello = HelloPayload {
        client_id: "act-as-tester".into(),
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

/// Encode `text` running as `act_as` (when `Some`), returning the assigned
/// `memory_id`. With `None` the op runs as the connection's own principal.
async fn encode_as(
    client: &mut TcpStream,
    stream_id: u32,
    text: &str,
    act_as: Option<ActAs>,
) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if opcode == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => panic!("encode failed: opcode=0x{opcode:04x} body={other:?}"),
    }
}

/// Recall running as `act_as` (when `Some`); returns the `memory_id`s in the
/// result set. Scope is always the effective identity — there is no client
/// filter widening it.
async fn recall_ids_as(
    client: &mut TcpStream,
    stream_id: u32,
    cue: &str,
    act_as: Option<ActAs>,
) -> Vec<u128> {
    let req = RecallRequest {
        trace: false,
        cue_text: cue.into(),
        subject_name: String::new(),
        max_results: 50,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
        act_as,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Recall(req)).await;
    assert_eq!(
        opcode,
        Opcode::RecallResp.as_u16(),
        "expected RecallResp, got 0x{opcode:04x}: {body:?}"
    );
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final, "v1 RECALL response must be final");
            r.memories.iter().map(|h| h.memory_id).collect()
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }
}

fn act_as(namespace: &str, space: [u8; 16]) -> ActAs {
    ActAs {
        namespace: namespace.to_string(),
        space_id: space,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Two tenants driven through ONE service-principal connection, distinguished
/// only by the per-request `act_as`, never see each other's memories. This is
/// the core guarantee behind the shared-pool gateway model: one authenticated
/// connection, per-request effective identity, hard tenant isolation.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn act_as_isolates_two_tenants_on_one_connection() {
    let server = start(1).await; // one shard → every identity collocated

    let space_a = [0xA1u8; 16];
    let space_b = [0xB2u8; 16];
    let svc_space = *uuid::Uuid::now_v7().as_bytes();

    // One trusted service principal: ACT_AS grant + an allowlist covering both
    // tenant namespaces. This is the only credential the "gateway" holds.
    let svc_token = server.mint_with_may_act(
        "svc",
        svc_space,
        brain_metadata::api_keys::bits::ACT_AS | brain_metadata::api_keys::bits::STANDARD_SPACE,
        vec!["tenant_a".to_string(), "tenant_b".to_string()],
    );

    let mut svc = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect svc");
    handshake_as(&mut svc, &svc_token).await;

    // Encode into tenant A and tenant B — same connection, different act_as.
    let a1 = encode_as(
        &mut svc,
        1,
        "tenant A private: the launch code is hunter2",
        Some(act_as("tenant_a", space_a)),
    )
    .await;
    let a2 = encode_as(
        &mut svc,
        3,
        "tenant A private: meet Priya at noon",
        Some(act_as("tenant_a", space_a)),
    )
    .await;
    let b1 = encode_as(
        &mut svc,
        5,
        "tenant B note: review the design doc",
        Some(act_as("tenant_b", space_b)),
    )
    .await;

    // Recall as tenant B → must see only B's row, never A's.
    let b_ids = recall_ids_as(
        &mut svc,
        7,
        "private launch code doc",
        Some(act_as("tenant_b", space_b)),
    )
    .await;
    assert!(
        !b_ids.contains(&a1) && !b_ids.contains(&a2),
        "ISOLATION BREACH: act_as=tenant_b recall returned tenant A's memory_id(s); \
         got {b_ids:?}, A owns [{a1}, {a2}]"
    );
    for id in &b_ids {
        assert_eq!(
            *id, b1,
            "act_as=tenant_b recall returned an id it doesn't own: {id} (B owns {b1})"
        );
    }

    // Recall as tenant A → must see only A's rows, never B's.
    let a_ids = recall_ids_as(
        &mut svc,
        9,
        "review design doc launch code",
        Some(act_as("tenant_a", space_a)),
    )
    .await;
    assert!(
        !a_ids.contains(&b1),
        "ISOLATION BREACH: act_as=tenant_a recall returned tenant B's memory {b1}; got {a_ids:?}"
    );
    for id in &a_ids {
        assert!(
            *id == a1 || *id == a2,
            "act_as=tenant_a recall returned an id it doesn't own: {id} (A owns [{a1}, {a2}])"
        );
    }

    server.stop().await;
}

/// R1: a principal WITHOUT the `ACT_AS` grant that supplies an `act_as` is
/// hard-rejected with `ActAsDenied` — never silently downgraded to run as its
/// own identity. Uses a plain FULL key (FULL excludes ACT_AS by construction).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn act_as_without_grant_is_denied() {
    let server = start(1).await;

    let space = [0xC3u8; 16];
    // FULL = ENCODE|RECALL|FORGET|LINK|SCHEMA_UPLOAD|ADMIN — deliberately no
    // ACT_AS bit, and no may_act allowlist.
    let token = server.mint("plain", space, brain_metadata::api_keys::bits::FULL);

    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake_as(&mut client, &token).await;

    let req = EncodeRequest {
        text: "should be rejected before it ever writes".into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: Some(act_as("tenant_a", [0xA1u8; 16])),
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    };
    let (opcode, body) = round_trip(&mut client, 1, RequestBody::Encode(req)).await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "expected an Error frame, got 0x{opcode:04x}: {body:?}"
    );
    match body {
        ResponseBody::Error(e) => assert_eq!(
            e.code,
            ErrorCodeWire::ActAsDenied,
            "expected ActAsDenied, got {:?}: {}",
            e.code,
            e.message
        ),
        other => panic!("expected Error body, got {other:?}"),
    }

    server.stop().await;
}

/// Wildcard `may_act = ["*"]`: a trusted front-door principal (gateway/edge)
/// may act as ANY namespace, including ones never named at mint time. This is
/// the multi-tenant SaaS case — the tenant set grows at runtime, so the service
/// principal can't enumerate an allowlist. Two arbitrary, unlisted namespaces
/// are each reachable through the one wildcard key, and stay isolated.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn act_as_wildcard_allows_any_namespace() {
    let server = start(1).await;

    let space_x = [0xD4u8; 16];
    let space_y = [0xE5u8; 16];
    let svc_space = *uuid::Uuid::now_v7().as_bytes();

    // Wildcard grant: ACT_AS + may_act = ["*"]. No tenant namespace is named.
    let svc_token = server.mint_with_may_act(
        "svc",
        svc_space,
        brain_metadata::api_keys::bits::ACT_AS | brain_metadata::api_keys::bits::STANDARD_SPACE,
        vec!["*".to_string()],
    );

    let mut svc = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect svc");
    handshake_as(&mut svc, &svc_token).await;

    // REACH: encoding into two never-listed namespaces must SUCCEED. `encode_as`
    // panics on any non-`EncodeResp` (an `ActAsDenied` would land here), so a
    // returned memory_id is itself the proof that the wildcard admitted a
    // namespace that was never named in `may_act`. Without the `"*"` grant these
    // two calls would be rejected by R2.
    let x1 = encode_as(
        &mut svc,
        1,
        "tenant X (unlisted): the vault combination is 4-2-9",
        Some(act_as("brand_new_tenant_x", space_x)),
    )
    .await;
    let y1 = encode_as(
        &mut svc,
        3,
        "tenant Y (unlisted): standup is at 9am",
        Some(act_as("brand_new_tenant_y", space_y)),
    )
    .await;

    // ISOLATION: reaching every namespace must not collapse tenant boundaries.
    // (The harness embeds zero vectors, so similarity ranking is degenerate;
    // like the other isolation tests we assert only the non-leak direction —
    // one tenant's recall never returns the other's memory_id.)
    let x_ids = recall_ids_as(
        &mut svc,
        5,
        "vault combination standup",
        Some(act_as("brand_new_tenant_x", space_x)),
    )
    .await;
    assert!(
        !x_ids.contains(&y1),
        "wildcard act_as broke isolation: tenant X's recall returned tenant Y's memory {y1}; got {x_ids:?}"
    );

    let y_ids = recall_ids_as(
        &mut svc,
        7,
        "vault combination standup",
        Some(act_as("brand_new_tenant_y", space_y)),
    )
    .await;
    assert!(
        !y_ids.contains(&x1),
        "wildcard act_as broke isolation: tenant Y's recall returned tenant X's memory {x1}; got {y_ids:?}"
    );

    server.stop().await;
}

/// R2: a principal WITH the `ACT_AS` grant that names a namespace OUTSIDE its
/// `may_act` allowlist is hard-rejected with `ActAsDenied`. The grant is not a
/// blanket impersonation right — it is bounded by the allowlist.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn act_as_outside_allowlist_is_denied() {
    let server = start(1).await;

    let svc_space = *uuid::Uuid::now_v7().as_bytes();
    // Grant ACT_AS but only for "tenant_a"; the request targets "tenant_x".
    let svc_token = server.mint_with_may_act(
        "svc",
        svc_space,
        brain_metadata::api_keys::bits::ACT_AS | brain_metadata::api_keys::bits::STANDARD_SPACE,
        vec!["tenant_a".to_string()],
    );

    let mut svc = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect svc");
    handshake_as(&mut svc, &svc_token).await;

    let req = EncodeRequest {
        text: "target namespace is not in may_act".into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: Some(act_as("tenant_x", [0x99u8; 16])),
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    };
    let (opcode, body) = round_trip(&mut svc, 1, RequestBody::Encode(req)).await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "expected an Error frame, got 0x{opcode:04x}: {body:?}"
    );
    match body {
        ResponseBody::Error(e) => assert_eq!(
            e.code,
            ErrorCodeWire::ActAsDenied,
            "expected ActAsDenied, got {:?}: {}",
            e.code,
            e.message
        ),
        other => panic!("expected Error body, got {other:?}"),
    }

    server.stop().await;
}

// ---------------------------------------------------------------------------
// SUBSCRIBE act_as (item 7a)
// ---------------------------------------------------------------------------

/// Positive: a service-principal connection issues SUBSCRIBE with `act_as`
/// targeting a DIFFERENT space it's permitted to `may_act` for, and receives
/// that space's events — not its own raw connection identity's. Two
/// connections authenticated with the SAME shared-pool key: one subscribes
/// (act_as = target), the other encodes (act_as = the SAME target), proving
/// the subscription is scoped to the effective identity end-to-end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_act_as_receives_target_spaces_events() {
    let server = start(1).await;

    let svc_space = *uuid::Uuid::now_v7().as_bytes();
    let target_space = [0xF6u8; 16];

    let svc_token = server.mint_with_may_act(
        "svc",
        svc_space,
        brain_metadata::api_keys::bits::ACT_AS | brain_metadata::api_keys::bits::STANDARD_SPACE,
        vec!["tenant_sub".to_string()],
    );

    // Subscriber connection: SUBSCRIBE act_as = the target space. The
    // `filter.spaces` is checked against the EFFECTIVE space, so it must
    // name the target, never the connection's own raw space.
    let mut sub = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect sub");
    handshake_as(&mut sub, &svc_token).await;

    let sub_req = SubscribeRequest {
        filter: SubscriptionFilter {
            contexts: None,
            kinds: None,
            similar_to: None,
            spaces: Some(vec![target_space]),
            memory_ids: None,
        },
        include_history: false,
        from_lsn: None,
        max_inflight: 100,
        act_as: Some(act_as("tenant_sub", target_space)),
    };
    send_frame(
        &mut sub,
        Frame::new(
            Opcode::SubscribeReq.as_u16(),
            FLAG_EOS,
            1,
            RequestBody::Subscribe(sub_req).encode(),
        ),
    )
    .await;
    // A successful SUBSCRIBE sends no synchronous opener frame; give the
    // registry a moment to register before the writer fires.
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Writer connection, same shared-pool key: ENCODE act_as = the SAME
    // target space, so the event is published under the effective identity
    // the subscription is scoped to.
    let mut writer = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect writer");
    handshake_as(&mut writer, &svc_token).await;
    let memory_id = encode_as(
        &mut writer,
        1,
        "act_as-scoped subscribe: real-time delegated event",
        Some(act_as("tenant_sub", target_space)),
    )
    .await;

    let mut got_event = false;
    for _ in 0..5 {
        let Some(frame) = read_frame_within(&mut sub, Duration::from_secs(2)).await else {
            break;
        };
        if frame.header.opcode_u16() == Opcode::Error.as_u16() {
            let body = ResponseBody::decode(Opcode::Error, &frame.payload).expect("decode");
            panic!("unexpected Error frame on act_as-scoped subscription: {body:?}");
        }
        if frame.header.opcode_u16() == Opcode::SubscribeEvent.as_u16() {
            let body = ResponseBody::decode(Opcode::SubscribeEvent, &frame.payload)
                .expect("decode subscribe event");
            if let ResponseBody::SubscribeEvent(ev) = body {
                if ev.event_type == EventType::Encoded && ev.memory_id == memory_id {
                    got_event = true;
                    break;
                }
            }
        }
    }
    assert!(
        got_event,
        "act_as-scoped SUBSCRIBE did not receive the target space's ENCODE event"
    );

    server.stop().await;
}

/// R1: a principal WITHOUT the `ACT_AS` grant that supplies a SUBSCRIBE
/// `act_as` selector is hard-rejected with `ActAsDenied` — the same code
/// and the same check `act_as_without_grant_is_denied` proves for ENCODE.
/// SUBSCRIBE reaches this check from its own structurally-separate dispatch
/// branch, so this proves the branch didn't silently skip R1.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_act_as_without_grant_is_denied() {
    let server = start(1).await;

    let space = [0xC4u8; 16];
    // FULL deliberately excludes ACT_AS.
    let token = server.mint("plain", space, brain_metadata::api_keys::bits::FULL);

    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    handshake_as(&mut client, &token).await;

    let req = SubscribeRequest {
        filter: SubscriptionFilter {
            contexts: None,
            kinds: None,
            similar_to: None,
            spaces: Some(vec![[0xA1u8; 16]]),
            memory_ids: None,
        },
        include_history: false,
        from_lsn: None,
        max_inflight: 100,
        act_as: Some(act_as("tenant_a", [0xA1u8; 16])),
    };
    let (opcode, body) = round_trip(&mut client, 1, RequestBody::Subscribe(req)).await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "expected an Error frame, got 0x{opcode:04x}: {body:?}"
    );
    match body {
        ResponseBody::Error(e) => assert_eq!(
            e.code,
            ErrorCodeWire::ActAsDenied,
            "expected ActAsDenied, got {:?}: {}",
            e.code,
            e.message
        ),
        other => panic!("expected Error body, got {other:?}"),
    }

    server.stop().await;
}

/// R2: a principal WITH the `ACT_AS` grant that names a SUBSCRIBE `act_as`
/// namespace OUTSIDE its `may_act` allowlist is hard-rejected with
/// `ActAsDenied` — the same code and the same check
/// `act_as_outside_allowlist_is_denied` proves for ENCODE.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscribe_act_as_outside_allowlist_is_denied() {
    let server = start(1).await;

    let svc_space = *uuid::Uuid::now_v7().as_bytes();
    let svc_token = server.mint_with_may_act(
        "svc",
        svc_space,
        brain_metadata::api_keys::bits::ACT_AS | brain_metadata::api_keys::bits::STANDARD_SPACE,
        vec!["tenant_a".to_string()],
    );

    let mut svc = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect svc");
    handshake_as(&mut svc, &svc_token).await;

    let req = SubscribeRequest {
        filter: SubscriptionFilter {
            contexts: None,
            kinds: None,
            similar_to: None,
            spaces: Some(vec![[0x99u8; 16]]),
            memory_ids: None,
        },
        include_history: false,
        from_lsn: None,
        max_inflight: 100,
        act_as: Some(act_as("tenant_x", [0x99u8; 16])),
    };
    let (opcode, body) = round_trip(&mut svc, 1, RequestBody::Subscribe(req)).await;
    assert_eq!(
        opcode,
        Opcode::Error.as_u16(),
        "expected an Error frame, got 0x{opcode:04x}: {body:?}"
    );
    match body {
        ResponseBody::Error(e) => assert_eq!(
            e.code,
            ErrorCodeWire::ActAsDenied,
            "expected ActAsDenied, got {:?}: {}",
            e.code,
            e.message
        ),
        other => panic!("expected Error body, got {other:?}"),
    }

    server.stop().await;
}
