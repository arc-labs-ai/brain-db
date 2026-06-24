//! Multi-agent isolation on the RECALL read path.
//!
//! The most important correctness property of a multi-tenant memory store:
//! one agent's RECALL must not return another agent's memories by default.
//! Brain enforces this at the filter level — every memory row carries its
//! owning `agent_id`, and when a RECALL arrives with an empty `agent_filter`
//! and `include_other_agents == false` the handler injects the caller's
//! authenticated agent as the scope. These tests prove that scoping holds, and
//! that the documented opt-ins (`include_other_agents`, explicit
//! `agent_filter`) widen it as specified.
//!
//! A single shard (`start(1)`) forces both agents onto shard 0 — `hash(agent)
//! % 1 == 0` — so what's under test is the *logical* per-agent filter, not the
//! incidental physical separation that distinct shards would provide.
//!
//! The test harness embeds with a zero-vector stub dispatcher, so similarity
//! ranking is degenerate; these tests assert only on *which* `memory_id`s
//! appear in a result set (membership), never on score ordering.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, RecallRequest, RequestBody,
};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
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
// Wire helpers
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

/// Handshake binding a specific `agent_id` (None-auth / permissive dev mode),
/// so each test connection is a distinct, controllable agent.
async fn handshake_as(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "isolation-tester".into(),
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
        agent_id,
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

/// Encode `text` and return the assigned `memory_id`.
async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if opcode == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => panic!("encode failed: opcode={opcode} body={other:?}"),
    }
}

/// Recall with explicit scope knobs; returns the `memory_id`s in the result set.
async fn recall_ids(
    client: &mut TcpStream,
    stream_id: u32,
    cue: &str,
    agent_filter: Vec<[u8; 16]>,
    include_other_agents: bool,
) -> Vec<u128> {
    let req = RecallRequest {
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
        agent_filter,
        include_other_agents,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Recall(req)).await;
    assert_eq!(
        opcode,
        Opcode::RecallResp.as_u16(),
        "expected RecallResp, got 0x{opcode:02x}: {body:?}"
    );
    match body {
        ResponseBody::Recall(r) => {
            assert!(r.is_final, "v1 RECALL response must be final");
            r.memories.iter().map(|h| h.memory_id).collect()
        }
        other => panic!("expected RecallResp, got {other:?}"),
    }
}

/// Retry a recall until it surfaces `wanted` or a deadline passes. The lexical
/// lane is populated by the async text-indexer, so a cross-agent recall fired
/// right after the other agent's ENCODE can race ahead of indexing; recall is
/// eventually consistent with encode. `recall_ids` mints a fresh request_id
/// per call, so idempotency never pins an early empty result.
async fn recall_ids_until_contains(
    client: &mut TcpStream,
    mut stream_id: u32,
    cue: &str,
    include_other_agents: bool,
    wanted: u128,
) -> Vec<u128> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    loop {
        let ids = recall_ids(client, stream_id, cue, Vec::new(), include_other_agents).await;
        if ids.contains(&wanted) || std::time::Instant::now() >= deadline {
            return ids;
        }
        stream_id += 2;
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// The core isolation guarantee: with the default scope (empty `agent_filter`,
/// `include_other_agents == false`), agent B's RECALL never returns a memory
/// that agent A encoded — even though both agents live on the same shard and
/// share one HNSW/tantivy index.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_recall_does_not_leak_other_agents_memories() {
    let server = start(1).await; // one shard → both agents collocated

    let agent_a = [0xAAu8; 16];
    let agent_b = [0xBBu8; 16];

    let mut a = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect a");
    let mut b = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect b");
    handshake_as(&mut a, agent_a).await;
    handshake_as(&mut b, agent_b).await;

    // Agent A stores private memories.
    let a1 = encode(&mut a, 1, "agent A private: the launch code is hunter2").await;
    let a2 = encode(&mut a, 3, "agent A private: meet Priya at noon").await;

    // Agent B stores its own, then recalls under the default scope.
    let b1 = encode(&mut b, 1, "agent B note: review the design doc").await;
    let b_ids = recall_ids(&mut b, 3, "private launch code doc", Vec::new(), false).await;

    // B must not see A's memories...
    assert!(
        !b_ids.contains(&a1) && !b_ids.contains(&a2),
        "ISOLATION BREACH: agent B's default recall returned agent A's memory_id(s); \
         got {b_ids:?}, A owns [{a1}, {a2}]"
    );
    // ...and the hits B does get must all be B's own.
    for id in &b_ids {
        assert_eq!(
            *id, b1,
            "agent B's default recall returned an id it doesn't own: {id} (B owns {b1})"
        );
    }

    // Symmetrically, A must not see B's memory.
    let a_ids = recall_ids(&mut a, 5, "review design doc note", Vec::new(), false).await;
    assert!(
        !a_ids.contains(&b1),
        "ISOLATION BREACH: agent A's default recall returned agent B's memory {b1}; got {a_ids:?}"
    );

    server.stop().await;
}

/// The documented opt-in: `include_other_agents = true` drops the implicit
/// caller scope, so a cross-agent recall can surface another agent's memory.
/// This proves the isolation in the test above is the *filter* doing its job,
/// not the zero-vector stub hiding everything.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn include_other_agents_opt_in_widens_scope() {
    let server = start(1).await;

    let agent_a = [0x11u8; 16];
    let agent_b = [0x22u8; 16];

    let mut a = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect a");
    let mut b = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect b");
    handshake_as(&mut a, agent_a).await;
    handshake_as(&mut b, agent_b).await;

    let a1 = encode(&mut a, 1, "shared-visible: quarterly numbers are up").await;

    // Default scope: B cannot see it.
    let scoped = recall_ids(&mut b, 1, "quarterly numbers", Vec::new(), false).await;
    assert!(
        !scoped.contains(&a1),
        "default scope must hide agent A's memory from B; got {scoped:?}"
    );

    // Opt in to cross-agent: B can now see it (once the index has caught up).
    let cross = recall_ids_until_contains(&mut b, 3, "quarterly numbers", true, a1).await;
    assert!(
        cross.contains(&a1),
        "include_other_agents=true must surface agent A's memory {a1} to B; got {cross:?}"
    );

    server.stop().await;
}
