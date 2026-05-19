//! Recall latency floor — `#[ignore]`-gated p95 regression gate.
//!
//! **PERF1** measures end-to-end client-observed RECALL latency on
//! the dev workstation. Two strategies, 100 iterations each:
//!
//! - **Auto (hybrid default)** — must hold p95 ≤ 12 ms. RECALL is
//!   what users feel most, and hybrid is the default path: a 12 ms
//!   p95 keeps interactive flows responsive at K=10 without text.
//! - **SubstrateOnly** — must hold p95 ≤ 1 ms. The substrate path
//!   over a 5-doc fixture has no HNSW pressure and no tantivy
//!   round-trip; this is the cache-warm floor.
//!
//! Gated behind `#[ignore]` because:
//!
//! 1. The 100-iteration loop dominates wall time in `cargo test`.
//! 2. The thresholds are workstation-tuned; CI hardware skew can
//!    legitimately blow past 12 ms without the underlying code
//!    being slower. The phase-23 acceptance suite is the
//!    production-reference gate.
//!
//! Run with: `cargo test -p brain-server --test recall_perf --
//! --ignored --test-threads=1`.

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use brain_protocol::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::opcode::Opcode;
use brain_protocol::request::{EncodeRequest, MemoryKindWire, RecallRequest, RecallStrategy};
use brain_protocol::response::ResponseBody;
use brain_protocol::Frame;
use brain_protocol::RequestBody;
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
const ITERATIONS: usize = 100;
const WARMUP_ITERATIONS: usize = 10;

// ---------------------------------------------------------------------------
// Wire helpers.
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
    let (frame, _) =
        Frame::decode_with_max(&buf, brain_protocol::MAX_PAYLOAD_BYTES as u32).expect("decode");
    frame
}

async fn send_frame(client: &mut TcpStream, frame: Frame) {
    client.write_all(&frame.encode()).await.expect("send");
    client.flush().await.expect("flush");
}

async fn complete_handshake(client: &mut TcpStream) {
    let hello = HelloPayload {
        client_id: "recall-perf".into(),
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

async fn encode_text(client: &mut TcpStream, stream_id: u32, text: &str) {
    let req = EncodeRequest {
        text: text.into(),
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    };
    let body = RequestBody::Encode(req);
    let opcode = body.opcode().as_u16();
    let payload = body.encode();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, payload)).await;
    let resp = read_one_frame(client).await;
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::EncodeResp.as_u16(),
        "encode failed",
    );
}

async fn seed_fixture(client: &mut TcpStream) {
    let phrases = [
        "Priya prefers async meetings over standups",
        "Async-first communication reduces context-switching",
        "Standups are a sync ritual we should retire",
        "Document driven design helps async teams",
        "Team prefers structured documents over live calls",
    ];
    for (i, p) in phrases.iter().enumerate() {
        encode_text(client, 101 + (i as u32) * 2, p).await;
    }
}

fn recall_request(strategy: Option<RecallStrategy>) -> RecallRequest {
    RecallRequest {
        cue_text: "meeting preferences".into(),
        cue_vector_offset: 0,
        cue_vector_dim: 0,
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        strategy,
        include_vectors: false,
        include_edges: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
    }
}

async fn one_recall(
    client: &mut TcpStream,
    stream_id: u32,
    strategy: Option<RecallStrategy>,
) -> Duration {
    let body = RequestBody::Recall(recall_request(strategy));
    let opcode = body.opcode().as_u16();
    let payload = body.encode();
    let start = Instant::now();
    send_frame(client, Frame::new(opcode, FLAG_EOS, stream_id, payload)).await;
    let resp = read_one_frame(client).await;
    let elapsed = start.elapsed();
    let resp_opcode = resp.header.opcode_u16();
    assert_eq!(
        resp_opcode,
        Opcode::RecallResp.as_u16(),
        "recall failed: opcode={resp_opcode}",
    );
    let decoded = ResponseBody::decode(
        Opcode::from_u16(resp_opcode).expect("known opcode"),
        &resp.payload,
    )
    .expect("decode resp");
    if let ResponseBody::Recall(r) = decoded {
        assert!(r.is_final, "non-final recall in perf loop");
    } else {
        panic!("expected RecallResp body");
    }
    elapsed
}

fn percentile(sorted_ns: &[u128], p: f64) -> Duration {
    assert!(!sorted_ns.is_empty());
    let rank = (p * (sorted_ns.len() as f64 - 1.0)).round() as usize;
    Duration::from_nanos(sorted_ns[rank.min(sorted_ns.len() - 1)] as u64)
}

async fn measure(
    client: &mut TcpStream,
    base_stream_id: u32,
    strategy: Option<RecallStrategy>,
) -> (Duration, Duration) {
    // Warm up: embedder cache, HNSW heuristics, allocator. A
    // production warm-up runs for minutes; here a brief loop is
    // enough because the workload is a 5-doc fixture and the
    // embedder is the only cold cache.
    let mut sid = base_stream_id;
    for _ in 0..WARMUP_ITERATIONS {
        let _ = one_recall(client, sid, strategy).await;
        sid += 2;
    }

    let mut samples_ns: Vec<u128> = Vec::with_capacity(ITERATIONS);
    for _ in 0..ITERATIONS {
        let d = one_recall(client, sid, strategy).await;
        sid += 2;
        samples_ns.push(d.as_nanos());
    }
    samples_ns.sort_unstable();
    let p50 = percentile(&samples_ns, 0.50);
    let p95 = percentile(&samples_ns, 0.95);
    (p50, p95)
}

// ---------------------------------------------------------------------------
// PERF1.
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
#[ignore = "perf gate: workstation-tuned thresholds, run explicitly"]
async fn recall_p95_meets_substrate_and_hybrid_targets() {
    let server = start(1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client).await;
    seed_fixture(&mut client).await;

    let (auto_p50, auto_p95) = measure(&mut client, 1001, Some(RecallStrategy::Auto)).await;
    let (sub_p50, sub_p95) =
        measure(&mut client, 10_001, Some(RecallStrategy::SubstrateOnly)).await;

    // Report regardless of pass/fail so a regression run still
    // surfaces the numbers in the test log.
    println!(
        "recall_p95: Auto p50={:?} p95={:?} | SubstrateOnly p50={:?} p95={:?} ({} samples each)",
        auto_p50, auto_p95, sub_p50, sub_p95, ITERATIONS,
    );

    assert!(
        auto_p95 <= Duration::from_millis(12),
        "Auto p95 = {auto_p95:?} exceeds 12 ms target",
    );
    assert!(
        sub_p95 <= Duration::from_millis(1),
        "SubstrateOnly p95 = {sub_p95:?} exceeds 1 ms target",
    );

    server.stop().await;
}
