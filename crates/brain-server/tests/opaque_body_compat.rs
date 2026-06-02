//! Substrate-only mode regression test.
//!
//! Proves that after the opaque-body storage extensions (new redb
//! tables, WAL frame kinds, on-disk paths, and `llm_cache.redb`) are
//! in place, the substrate primitives still work end-to-end **and**
//! the phase bodies stays dormant when no schema is declared.
//!
//! ## What's asserted
//!
//! 1. ENCODE / RECALL / FORGET round-trip over the wire without error.
//! 2. The 25 opaque-body redb tables remain empty after the
//!    workload (no behavior accidentally wrote into them).
//! 3. The WAL contains zero opaque-body frames (none of the
//!    `0x10..=0x50` discriminants ever produced).
//! 4. `llm_cache.redb` opens and both cache tables exist + are empty.
//!
//! ## Latency
//!
//! Per-op p50/p99 of ENCODE+RECALL are logged for visibility. The
//! only assertion is a loose backstop: p99 < 500 ms. Tight `≤110%
//! of baseline` thresholds need quiet reference hardware + a
//! committed baseline file; both are operator-cadence concerns for
//! substrate acceptance. This test only catches catastrophic
//! regressions.
//!
//! ## Binding
//!
//! Schema-optional behavior must be byte-identical to a
//! pre-opaque-body deployment. (2) and (3) above are how we
//! prove that on-disk and in-flight respectively.

#![cfg(target_os = "linux")]

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

use std::time::{Duration, Instant};

use brain_metadata::tables::audit::{ENTITY_RESOLUTION_AUDIT_TABLE, EXTRACTOR_AUDIT_TABLE};
use brain_metadata::tables::entity::{
    ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE, ENTITY_MENTIONS_TABLE,
    ENTITY_TRIGRAMS_TABLE,
};
use brain_metadata::tables::entity_type::ENTITY_TYPES_TABLE;
use brain_metadata::tables::extractor::EXTRACTORS_TABLE;
use brain_metadata::tables::merge::MERGE_LOG_TABLE;
use brain_metadata::tables::predicate::PREDICATES_TABLE;
use brain_metadata::tables::relation::{RELATION_BY_EVIDENCE_TABLE, RELATION_METADATA_TABLE};
use brain_metadata::tables::relation_type::RELATION_TYPES_TABLE;
use brain_metadata::tables::schema_version::SCHEMA_VERSIONS_TABLE;
use brain_metadata::tables::statement::{
    EVIDENCE_OVERFLOW_TABLE, STATEMENTS_BY_EVENT_TIME_TABLE, STATEMENTS_BY_EVIDENCE_TABLE,
    STATEMENTS_BY_OBJECT_ENTITY_TABLE, STATEMENTS_BY_PREDICATE_TABLE, STATEMENTS_BY_SUBJECT_TABLE,
    STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};
use brain_metadata::LlmCacheDb;
use brain_protocol::codec::opcode::Opcode;
use brain_protocol::connection::handshake::{
    AuthCredentials, AuthMethod, AuthPayload, HelloCapabilities, HelloPayload,
};
use brain_protocol::envelope::request::{
    ByeRequest, EncodeRequest, ForgetMode, ForgetRequest, MemoryKindWire, RecallRequest,
    RequestBody,
};
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::Frame;
use brain_storage::ShardPaths;
use redb::ReadableDatabase;
use redb::ReadableTable;
use tempfile::TempDir;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use support_harness::start_in;

const FLAG_EOS: u8 = 1 << 7;

// ---------------------------------------------------------------------------
// Wire helpers (mirror tests/e2e.rs).
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

async fn complete_handshake(client: &mut TcpStream, agent_id: [u8; 16]) {
    let hello = HelloPayload {
        client_id: "opaque-body-compat".into(),
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

async fn encode_round_trip(client: &mut TcpStream, stream_id: u32, text: String) -> u128 {
    let req = EncodeRequest {
        text,
        context_id: 0,
        kind: MemoryKindWire::Episodic,
        salience_hint: 0.5,
        edges: Vec::new(),
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
        deduplicate: false,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::EncodeReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Encode(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await;
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::EncodeResp.as_u16(),
        "encode resp opcode"
    );
    match ResponseBody::decode(Opcode::EncodeResp, &resp.payload).expect("decode encode resp") {
        ResponseBody::Encode(r) => r.memory_id,
        other => panic!("expected EncodeResp, got {other:?}"),
    }
}

async fn recall_round_trip(client: &mut TcpStream, stream_id: u32, cue: String) {
    let req = RecallRequest {
        cue_text: cue,
        top_k: 5,
        confidence_threshold: 0.0,
        context_filter: None,
        age_bound_unix_nanos: None,
        kind_filter: None,
        salience_floor: 0.0,
        include_edges: false,
        include_graph: false,
        include_text: false,
        request_id: Some(*uuid::Uuid::now_v7().as_bytes()),
        txn_id: None,
        agent_filter: Vec::new(),
        include_other_agents: false,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::RecallReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Recall(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await;
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::RecallResp.as_u16(),
        "recall resp opcode"
    );
}

async fn forget_round_trip(client: &mut TcpStream, stream_id: u32, memory_id: u128) -> u128 {
    let req = ForgetRequest {
        memory_id,
        mode: ForgetMode::Soft,
        request_id: *uuid::Uuid::now_v7().as_bytes(),
        txn_id: None,
    };
    send_frame(
        client,
        Frame::new(
            Opcode::ForgetReq.as_u16(),
            FLAG_EOS,
            stream_id,
            RequestBody::Forget(req).encode(),
        ),
    )
    .await;
    let resp = read_one_frame(client).await;
    assert_eq!(
        resp.header.opcode_u16(),
        Opcode::ForgetResp.as_u16(),
        "forget resp opcode"
    );
    match ResponseBody::decode(Opcode::ForgetResp, &resp.payload).expect("decode forget resp") {
        ResponseBody::Forget(r) => r.memory_id,
        other => panic!("expected ForgetResp, got {other:?}"),
    }
}

async fn bye(client: &mut TcpStream) {
    send_frame(
        client,
        Frame::new(
            Opcode::Bye.as_u16(),
            FLAG_EOS,
            0,
            RequestBody::Bye(ByeRequest {
                reason: Some("opaque-body-compat done".into()),
            })
            .encode(),
        ),
    )
    .await;
}

const ENCODE_COUNT: usize = 50;
const RECALL_COUNT: usize = 20;
const FORGET_COUNT: usize = 5;
const P99_BACKSTOP: Duration = Duration::from_millis(500);

/// Drives the substrate's hot paths and asserts
/// the phase bodies stayed dormant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_off_substrate_round_trips_and_keeps_graph_dormant() {
    // Caller-owned data dir so we can inspect on-disk state after the
    // server stops.
    let data_dir = TempDir::new().expect("tmp data dir");
    let server = start_in(data_dir.path(), 1).await;
    let mut client = TcpStream::connect(server.data_plane_addr)
        .await
        .expect("connect");
    complete_handshake(&mut client, *uuid::Uuid::now_v7().as_bytes()).await;

    // ---- 1. ENCODE round-trip --------------------------------------
    let mut encoded_ids: Vec<u128> = Vec::with_capacity(ENCODE_COUNT);
    let mut encode_latencies: Vec<Duration> = Vec::with_capacity(ENCODE_COUNT);
    let mut stream_id: u32 = 1;
    for i in 0..ENCODE_COUNT {
        let text = format!("no-schema test fixture, memory index {i:03} of {ENCODE_COUNT}");
        let start = Instant::now();
        let memory_id = encode_round_trip(&mut client, stream_id, text).await;
        encode_latencies.push(start.elapsed());
        assert_ne!(memory_id, 0, "encoded id must be non-null");
        encoded_ids.push(memory_id);
        stream_id += 2;
    }
    assert_eq!(encoded_ids.len(), ENCODE_COUNT);

    // ---- 2. RECALL round-trip (protocol level only) ----------------
    //
    // We don't assert "recall returns the encoded memory" — the
    // harness's dispatcher path doesn't guarantee semantic correctness
    // under the test config. The contract asserted here: every RECALL
    // round-trips and returns a RECALL_RESP.
    let mut recall_latencies: Vec<Duration> = Vec::with_capacity(RECALL_COUNT);
    for i in 0..RECALL_COUNT {
        let cue = format!("fixture memory {i:03}");
        let start = Instant::now();
        recall_round_trip(&mut client, stream_id, cue).await;
        recall_latencies.push(start.elapsed());
        stream_id += 2;
    }

    // ---- 3. FORGET round-trip --------------------------------------
    for &memory_id in encoded_ids.iter().take(FORGET_COUNT) {
        let echoed = forget_round_trip(&mut client, stream_id, memory_id).await;
        assert_eq!(echoed, memory_id);
        stream_id += 2;
    }

    bye(&mut client).await;
    server.stop().await;

    // ---- 4. Typed-graph tables must remain empty ---------------------
    //
    // Open metadata.redb directly (the server's exclusive lock was
    // released on stop) and assert every opaque-body table has
    // zero rows.
    let paths = ShardPaths::at(data_dir.path().join("0"));
    let metadata = redb::Database::open(paths.metadata_db()).expect("open metadata.redb");
    let rtxn = metadata.begin_read().expect("read txn");

    assert_table_empty(&rtxn, ENTITIES_TABLE, "entities");
    assert_table_empty(
        &rtxn,
        ENTITY_BY_CANONICAL_NAME_TABLE,
        "entity_by_canonical_name",
    );
    assert_table_empty(&rtxn, ENTITY_ALIASES_TABLE, "entity_aliases");
    assert_table_empty(&rtxn, ENTITY_TRIGRAMS_TABLE, "entity_trigrams");
    assert_table_empty(&rtxn, ENTITY_MENTIONS_TABLE, "entity_mentions");

    assert_table_empty(&rtxn, STATEMENTS_TABLE, "statements");
    assert_table_empty(&rtxn, STATEMENTS_BY_SUBJECT_TABLE, "statements_by_subject");
    assert_table_empty(
        &rtxn,
        STATEMENTS_BY_PREDICATE_TABLE,
        "statements_by_predicate",
    );
    assert_table_empty(
        &rtxn,
        STATEMENTS_BY_OBJECT_ENTITY_TABLE,
        "statements_by_object_entity",
    );
    assert_table_empty(
        &rtxn,
        STATEMENTS_BY_EVENT_TIME_TABLE,
        "statements_by_event_time",
    );
    assert_table_empty(
        &rtxn,
        STATEMENTS_BY_EVIDENCE_TABLE,
        "statements_by_evidence",
    );
    assert_table_empty(&rtxn, STATEMENT_CHAIN_TABLE, "statement_chain");
    assert_table_empty(&rtxn, EVIDENCE_OVERFLOW_TABLE, "evidence_overflow");

    assert_table_empty(&rtxn, RELATION_METADATA_TABLE, "relation_metadata");
    assert_table_empty(&rtxn, RELATION_BY_EVIDENCE_TABLE, "relation_by_evidence");

    assert_table_empty(&rtxn, PREDICATES_TABLE, "predicates");
    assert_table_empty(&rtxn, ENTITY_TYPES_TABLE, "entity_types");
    assert_table_empty(&rtxn, RELATION_TYPES_TABLE, "relation_types");
    assert_table_empty(&rtxn, EXTRACTORS_TABLE, "extractors");
    assert_table_empty(&rtxn, SCHEMA_VERSIONS_TABLE, "schema_versions");
    assert_table_empty(&rtxn, EXTRACTOR_AUDIT_TABLE, "extractor_audit");
    assert_table_empty(
        &rtxn,
        ENTITY_RESOLUTION_AUDIT_TABLE,
        "entity_resolution_audit",
    );
    assert_table_empty(&rtxn, MERGE_LOG_TABLE, "merge_log");

    drop(rtxn);
    drop(metadata);

    // ---- 5. WAL must contain zero typed-graph frames -----------------
    //
    // Iterate every record and assert `!kind.has_opaque_body()`. Substrate
    // records (Encode / Forget / CheckpointBegin / CheckpointEnd / …)
    // are expected; typed-graph records (0x10..=0x50) must be zero.
    let shard_uuid = std::fs::read(paths.shard_uuid()).expect("shard.uuid");
    let shard_uuid: [u8; 16] = shard_uuid
        .as_slice()
        .try_into()
        .expect("shard.uuid is 16 bytes");
    let wal_reader =
        brain_storage::wal::reader::WalReader::open(paths.wal_dir(), shard_uuid).expect("wal");

    let mut wal_records_seen: u64 = 0;
    let mut graph_records_seen: u64 = 0;
    for item in wal_reader {
        let record = item.expect("wal record decode");
        wal_records_seen += 1;
        if record.kind.has_opaque_body() {
            graph_records_seen += 1;
        }
    }
    assert!(
        wal_records_seen > 0,
        "expected substrate WAL records to be present; found 0"
    );
    assert_eq!(
        graph_records_seen, 0,
        "schema-off run produced {graph_records_seen} typed-graph WAL records; expected 0"
    );

    // ---- 6. llm_cache.redb opens and is empty ----------------------
    let cache = LlmCacheDb::open(paths.llm_cache_db()).expect("open llm_cache.redb");
    let rtxn = cache.read_txn().expect("cache read txn");
    let responses = rtxn
        .open_table(brain_metadata::llm_cache::LLM_RESPONSES_TABLE)
        .expect("responses table");
    let ttl = rtxn
        .open_table(brain_metadata::llm_cache::LLM_RESPONSE_TTL_TABLE)
        .expect("ttl table");
    assert_eq!(
        responses.iter().expect("scan").count(),
        0,
        "llm_responses must be empty on schema-off run"
    );
    assert_eq!(
        ttl.iter().expect("scan").count(),
        0,
        "llm_response_ttl must be empty on schema-off run"
    );

    // ---- 7. Latency smoke (loose backstop) -------------------------
    let encode_p99 = quantile(&mut encode_latencies, 0.99);
    let recall_p99 = quantile(&mut recall_latencies, 0.99);
    let encode_p50 = quantile(&mut encode_latencies, 0.50);
    let recall_p50 = quantile(&mut recall_latencies, 0.50);
    tracing::info!(
        encode_p50_ms = encode_p50.as_millis(),
        encode_p99_ms = encode_p99.as_millis(),
        recall_p50_ms = recall_p50.as_millis(),
        recall_p99_ms = recall_p99.as_millis(),
        "phase-15.5 substrate latency smoke"
    );
    assert!(
        encode_p99 < P99_BACKSTOP,
        "ENCODE p99 {encode_p99:?} exceeded backstop {P99_BACKSTOP:?}; \
         catastrophic regression — see spec/16/02 for reference targets"
    );
    assert!(
        recall_p99 < P99_BACKSTOP,
        "RECALL p99 {recall_p99:?} exceeded backstop {P99_BACKSTOP:?}; \
         catastrophic regression — see spec/16/02 for reference targets"
    );
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

/// Assert a redb table is empty in the given read transaction.
///
/// Uses an iterator count rather than a typed `is_empty()` since redb's
/// `Table::is_empty` is fallible and the count-based form gives a more
/// useful diagnostic on failure.
fn assert_table_empty<K, V>(
    rtxn: &redb::ReadTransaction,
    table_def: redb::TableDefinition<'_, K, V>,
    name: &str,
) where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    let table = rtxn
        .open_table(table_def)
        .unwrap_or_else(|e| panic!("open {name}: {e}"));
    let count = table
        .iter()
        .unwrap_or_else(|e| panic!("scan {name}: {e}"))
        .count();
    assert_eq!(
        count, 0,
        "opaque-body table `{name}` should be empty on schema-off run; found {count} entries"
    );
}

/// `samples`-based quantile (0.0..=1.0). Sorts in place. Returns
/// `Duration::ZERO` on empty input — caller is responsible for not
/// asserting against an empty distribution.
fn quantile(samples: &mut [Duration], q: f64) -> Duration {
    if samples.is_empty() {
        return Duration::ZERO;
    }
    samples.sort();
    let idx = ((samples.len() as f64) * q) as usize;
    let idx = idx.min(samples.len() - 1);
    samples[idx]
}
