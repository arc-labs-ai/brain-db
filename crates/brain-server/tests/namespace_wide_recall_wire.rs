//! End-to-end wire test for namespace-wide RECALL (Phase C: cross-shard
//! fan-out + global merge).
//!
//! A namespace's spaces are spread across shards. A namespace-wide RECALL
//! (`scope = Namespace`) must therefore fan out to EVERY shard, gather each
//! shard's raw candidate pool, merge them, and shape once — returning the
//! caller's memories across ALL its spaces (even those living on other shards)
//! and NEVER another tenant's. This drives that path through the real server:
//! two shards, two `acme` spaces deliberately placed on different shards, plus
//! a `globex` tenant that must never surface.
//!
//! The default (`scope = Space`) recall is unchanged — it stays pinned to the
//! calling key's own space and is served by a single shard.

#![cfg(target_os = "linux")]

use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthOkPayload, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    EncodeRequest, RecallRequest, RecallScopeWire, RequestBody,
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

async fn handshake_authok(client: &mut TcpStream, token: &[u8]) -> AuthOkPayload {
    let hello = HelloPayload {
        client_id: "ns-wide-recall-tester".into(),
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
    assert_eq!(
        auth_ok.header.opcode_u16(),
        Opcode::AuthOk.as_u16(),
        "expected AuthOk, got 0x{:04x}",
        auth_ok.header.opcode_u16()
    );
    AuthOkPayload::decode(&auth_ok.payload).expect("decode AuthOk")
}

async fn encode(client: &mut TcpStream, stream_id: u32, text: &str) -> u128 {
    let req = EncodeRequest {
        text: text.into(),
        session_id: 0,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    };
    let (opcode, body) = round_trip(client, stream_id, RequestBody::Encode(req)).await;
    match body {
        ResponseBody::Encode(r) if opcode == Opcode::EncodeResp.as_u16() => r.memory_id,
        other => panic!("encode failed: opcode={opcode} body={other:?}"),
    }
}

fn recall_request(cue: &str, scope: RecallScopeWire) -> RecallRequest {
    RecallRequest {
        scope,
        trace: false,
        cue_text: cue.into(),
        subject_name: String::new(),
        max_results: 20,
        confidence_threshold: 0.0,
        session_filter: None,
        age_bound_unix_nanos: None,
        as_of_record_time_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: true,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
        act_as: None,
    }
}

/// Re-issue RECALL until `done(&frame)` holds or a deadline passes. The lexical
/// lane is populated by the async text-indexer on a commit cadence — and under a
/// namespace-wide fan-out each shard indexes independently — so a recall fired
/// immediately after ENCODE can race indexing on some shards. Polling until the
/// expected cross-shard result is present (not merely non-empty) makes the test
/// robust to per-shard indexing lag. Each attempt mints a fresh request_id so
/// RECALL idempotency never pins an early partial result.
async fn recall_until<F>(
    client: &mut TcpStream,
    stream_id: u32,
    cue: &str,
    scope: RecallScopeWire,
    done: F,
) -> brain_protocol::RecallResponseFrame
where
    F: Fn(&brain_protocol::RecallResponseFrame) -> bool,
{
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let (opcode, body) = round_trip(
            client,
            stream_id,
            RequestBody::Recall(recall_request(cue, scope)),
        )
        .await;
        assert_eq!(
            opcode,
            Opcode::RecallResp.as_u16(),
            "recall failed: {body:?}"
        );
        let ResponseBody::Recall(frame) = body else {
            panic!("expected RecallResp");
        };
        if done(&frame) || std::time::Instant::now() >= deadline {
            return frame;
        }
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    }
}

fn has_text(frame: &brain_protocol::RecallResponseFrame, needle: &str) -> bool {
    frame.memories.iter().any(|m| m.text.contains(needle))
}

/// Find two space ids that hash to DIFFERENT shards under `n_shards`, so an
/// `acme` namespace genuinely straddles shards and the fan-out is exercised.
fn two_spaces_on_distinct_shards(n_shards: u16) -> ([u8; 16], [u8; 16]) {
    let first = *uuid::Uuid::now_v7().as_bytes();
    let first_shard = routing::hash_space_to_shard(brain_core::SpaceId::from(first), n_shards);
    loop {
        let candidate = *uuid::Uuid::now_v7().as_bytes();
        let shard = routing::hash_space_to_shard(brain_core::SpaceId::from(candidate), n_shards);
        if shard != first_shard {
            return (first, candidate);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Namespace-wide RECALL returns a tenant's memories across ALL its spaces —
/// including spaces on other shards — and never another tenant's, while the
/// default space-scoped RECALL stays pinned to the calling key's own space.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn namespace_wide_recall_spans_shards_never_crosses_namespace() {
    let server = start(2).await; // two shards → an acme space per shard

    let (space_a1, space_a2) = two_spaces_on_distinct_shards(2);
    let space_b = *uuid::Uuid::now_v7().as_bytes();

    let full = brain_metadata::api_keys::bits::FULL;
    let acme_k1 = server.mint("acme", space_a1, full);
    let acme_k2 = server.mint("acme", space_a2, full);
    let globex_k = server.mint("globex", space_b, full);

    // Encode a memory sharing the cue token in each acme space, and one under
    // globex that also matches the cue token (so only the tenant wall — not the
    // cue — can keep it out).
    {
        let mut c = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect a1");
        handshake_authok(&mut c, &acme_k1).await;
        encode(&mut c, 1, "the quarterly vault passphrase is alpha-one").await;
    }
    {
        let mut c = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect a2");
        handshake_authok(&mut c, &acme_k2).await;
        encode(&mut c, 1, "the quarterly vault passphrase is alpha-two").await;
    }
    {
        let mut c = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect b");
        handshake_authok(&mut c, &globex_k).await;
        encode(&mut c, 1, "the quarterly vault passphrase is omega").await;
    }

    // (a) Namespace-wide recall from the acme space-1 key spans BOTH acme spaces.
    let mut acme = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect acme");
    handshake_authok(&mut acme, &acme_k1).await;
    // Wait until BOTH acme spaces (one per shard) have surfaced — the whole
    // point of the cross-shard fan-out — rather than the first shard to index.
    let frame = recall_until(
        &mut acme,
        1,
        "quarterly vault passphrase",
        RecallScopeWire::Namespace,
        |f| has_text(f, "alpha-one") && has_text(f, "alpha-two"),
    )
    .await;

    // The memory TEXT is the ground truth of which space each hit came from
    // (each text was encoded under exactly one key/space). The minted `space`
    // bytes are not the caller's effective SpaceId, so we assert on text + on
    // space-id distinctness rather than on the raw mint bytes.
    let texts: Vec<&str> = frame.memories.iter().map(|m| m.text.as_str()).collect();
    assert!(
        texts.iter().any(|t| t.contains("alpha-one")),
        "namespace-wide recall must include the caller's OWN space memory; got {texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("alpha-two")),
        "namespace-wide recall must include a SIBLING space on another shard \
         (cross-shard fan-out); got {texts:?}"
    );
    // TENANT WALL: globex's memory (same cue token) must never surface.
    assert!(
        !texts.iter().any(|t| t.contains("omega")),
        "TENANCY BREACH: namespace-wide recall leaked a foreign namespace's memory; got {texts:?}"
    );
    // Cross-space proof: the two acme hits carry two DISTINCT space ids.
    let distinct_spaces: std::collections::HashSet<[u8; 16]> =
        frame.memories.iter().map(|m| m.space_id).collect();
    assert!(
        distinct_spaces.len() >= 2,
        "namespace-wide recall must span >= 2 distinct spaces; got {distinct_spaces:?}"
    );

    // (b) The default (space-scoped) recall from the SAME key stays pinned to
    // its own space — it does not fan out.
    let scoped = recall_until(
        &mut acme,
        3,
        "quarterly vault passphrase",
        RecallScopeWire::Space,
        |f| has_text(f, "alpha-one"),
    )
    .await;
    let scoped_texts: Vec<&str> = scoped.memories.iter().map(|m| m.text.as_str()).collect();
    assert!(
        scoped_texts.iter().any(|t| t.contains("alpha-one")),
        "space-scoped recall must return the caller's own space memory; got {scoped_texts:?}"
    );
    assert!(
        !scoped_texts.iter().any(|t| t.contains("alpha-two")),
        "space-scoped recall must NOT return a sibling space (no fan-out); got {scoped_texts:?}"
    );
    assert!(
        !scoped_texts.iter().any(|t| t.contains("omega")),
        "space-scoped recall must never return a foreign namespace's memory; got {scoped_texts:?}"
    );
    // All space-scoped hits share the ONE calling space.
    let scoped_spaces: std::collections::HashSet<[u8; 16]> =
        scoped.memories.iter().map(|m| m.space_id).collect();
    assert_eq!(
        scoped_spaces.len(),
        1,
        "space-scoped recall must stay within a single space; got {scoped_spaces:?}"
    );

    server.stop().await;
}

/// The tenant wall holds against MULTIPLE foreign tenants, and the fan-out
/// tolerates shards that hold none of the caller's rows (empty gather pools).
/// Three namespaces on a 3-shard server, each with a memory sharing the cue
/// token: a namespace-wide recall from EACH tenant returns only its own memory
/// and never either of the other two — whichever shards those happened to land
/// on, including shards contributing an empty pool to the merge.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn namespace_wide_recall_walls_multiple_foreign_tenants() {
    let server = start(3).await;
    let full = brain_metadata::api_keys::bits::FULL;

    // One tenant, one space, one memory. Each memory shares the cue token so
    // only the namespace wall — not the cue — can keep foreign ones out.
    let tenants = [
        ("acme", "the shared launch codeword is acme-secret"),
        ("globex", "the shared launch codeword is globex-secret"),
        ("initech", "the shared launch codeword is initech-secret"),
    ];
    let mut keys = Vec::new();
    for (ns, text) in tenants {
        let key = server.mint(ns, *uuid::Uuid::now_v7().as_bytes(), full);
        let mut c = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect");
        handshake_authok(&mut c, &key).await;
        encode(&mut c, 1, text).await;
        keys.push((ns, key));
    }

    for (ns, key) in &keys {
        let mut c = TcpStream::connect(server.data_plane_addr)
            .await
            .expect("connect");
        handshake_authok(&mut c, key).await;
        let own = format!("{ns}-secret");
        let frame = recall_until(
            &mut c,
            1,
            "shared launch codeword",
            RecallScopeWire::Namespace,
            |f| has_text(f, &own),
        )
        .await;
        let texts: Vec<&str> = frame.memories.iter().map(|m| m.text.as_str()).collect();
        assert!(
            texts.iter().any(|t| t.contains(&own)),
            "{ns}: namespace-wide recall must return its own memory; got {texts:?}"
        );
        for (other_ns, _) in &keys {
            if other_ns == ns {
                continue;
            }
            let foreign = format!("{other_ns}-secret");
            assert!(
                !texts.iter().any(|t| t.contains(&foreign)),
                "TENANCY BREACH: {ns} namespace-wide recall leaked {other_ns}'s memory; got {texts:?}"
            );
        }
    }

    server.stop().await;
}
