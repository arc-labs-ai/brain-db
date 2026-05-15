//! Substrate-only mode regression test. Sub-task 15.5.
//!
//! Closes Phase 15 (knowledge-layer storage extensions). Proves that
//! after the new redb tables (15.1), WAL frame kinds (15.2), on-disk
//! paths (15.3), and `llm_cache.redb` (15.4) are in place, the
//! substrate primitives still work end-to-end **and** the knowledge
//! layer stays dormant when no schema is declared.
//!
//! ## What's asserted
//!
//! 1. ENCODE / RECALL / FORGET round-trip via the SDK without error.
//! 2. The 25 knowledge-layer redb tables remain empty after the
//!    workload (no behavior accidentally wrote into them).
//! 3. The WAL contains zero knowledge-layer frames (none of the
//!    `0x10..=0x50` discriminants from 15.2 ever produced).
//! 4. `llm_cache.redb` opens and both cache tables exist + are empty.
//!
//! ## Latency
//!
//! Per-op p50/p99 of ENCODE+RECALL are logged for visibility. The
//! only assertion is a loose backstop: p99 < 500 ms. Tight `≤110%
//! of baseline` thresholds need quiet reference hardware + a
//! committed baseline file; both are operator-cadence concerns for
//! Phase 14 (substrate acceptance). 15.5 only catches catastrophic
//! regressions.
//!
//! ## AUTONOMY §23 binding
//!
//! Schema-optional behavior must be byte-identical to a
//! pre-knowledge-layer deployment. (2) and (3) above are how we
//! prove that on-disk and in-flight respectively.

#![cfg(target_os = "linux")]

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

use std::time::{Duration, Instant};

use brain_core::MemoryId;
use brain_metadata::tables::knowledge::audit::{
    ENTITY_RESOLUTION_AUDIT_TABLE, EXTRACTOR_AUDIT_TABLE,
};
use brain_metadata::tables::knowledge::entity::{
    ENTITIES_TABLE, ENTITY_ALIASES_TABLE, ENTITY_BY_CANONICAL_NAME_TABLE, ENTITY_MENTIONS_TABLE,
    ENTITY_TRIGRAMS_TABLE,
};
use brain_metadata::tables::knowledge::entity_type::ENTITY_TYPES_TABLE;
use brain_metadata::tables::knowledge::extractor::EXTRACTORS_TABLE;
use brain_metadata::tables::knowledge::merge::MERGE_LOG_TABLE;
use brain_metadata::tables::knowledge::predicate::PREDICATES_TABLE;
use brain_metadata::tables::knowledge::relation::{
    RELATIONS_BY_EVIDENCE_TABLE, RELATIONS_BY_FROM_TABLE, RELATIONS_BY_TO_TABLE, RELATIONS_TABLE,
};
use brain_metadata::tables::knowledge::relation_type::RELATION_TYPES_TABLE;
use brain_metadata::tables::knowledge::schema_version::SCHEMA_VERSIONS_TABLE;
use brain_metadata::tables::knowledge::statement::{
    EVIDENCE_OVERFLOW_TABLE, STATEMENTS_BY_EVENT_TIME_TABLE, STATEMENTS_BY_EVIDENCE_TABLE,
    STATEMENTS_BY_OBJECT_ENTITY_TABLE, STATEMENTS_BY_PREDICATE_TABLE, STATEMENTS_BY_SUBJECT_TABLE,
    STATEMENTS_TABLE, STATEMENT_CHAIN_TABLE,
};
use brain_metadata::LlmCacheDb;
use brain_protocol::request::ForgetMode;
use brain_sdk_rust::Client;
use brain_storage::ShardPaths;
use redb::ReadableDatabase;
use redb::ReadableTable;
use tempfile::TempDir;

use support_harness::start_in;

const ENCODE_COUNT: usize = 50;
const RECALL_COUNT: usize = 20;
const FORGET_COUNT: usize = 5;
const P99_BACKSTOP: Duration = Duration::from_millis(500);

/// Phase 15 closure test. Drives the substrate's hot paths and asserts
/// the knowledge layer stayed dormant.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn schema_off_substrate_round_trips_and_keeps_knowledge_dormant() {
    // Caller-owned data dir so we can inspect on-disk state after the
    // server stops.
    let data_dir = TempDir::new().expect("tmp data dir");
    let server = start_in(data_dir.path(), 1).await;
    let client = Client::connect(server.data_plane_addr)
        .await
        .expect("connect");

    // ---- 1. ENCODE round-trip --------------------------------------
    let mut encoded_ids: Vec<MemoryId> = Vec::with_capacity(ENCODE_COUNT);
    let mut encode_latencies: Vec<Duration> = Vec::with_capacity(ENCODE_COUNT);
    for i in 0..ENCODE_COUNT {
        let text = format!(
            "phase-15.5 substrate-only test fixture, memory index {i:03} of {ENCODE_COUNT}"
        );
        let start = Instant::now();
        let resp = client.encode(text).send().await.expect("encode");
        encode_latencies.push(start.elapsed());
        assert_ne!(resp.memory_id, 0, "encoded id must be non-null");
        encoded_ids.push(MemoryId::from_raw(resp.memory_id));
    }
    assert_eq!(encoded_ids.len(), ENCODE_COUNT);

    // ---- 2. RECALL round-trip (protocol level only) ----------------
    //
    // Per the established e2e-test convention (`sdk_e2e.rs`), we don't
    // assert "recall returns the encoded memory" — the harness's
    // dispatcher path doesn't guarantee semantic correctness under
    // the test config. The contract being asserted here: every RECALL
    // returns Ok and the SDK + server round-trip succeeds.
    let mut recall_latencies: Vec<Duration> = Vec::with_capacity(RECALL_COUNT);
    for i in 0..RECALL_COUNT {
        let cue = format!("fixture memory {i:03}");
        let start = Instant::now();
        let _results = client.recall(cue).send().await.expect("recall");
        recall_latencies.push(start.elapsed());
    }

    // ---- 3. FORGET round-trip --------------------------------------
    for memory_id in encoded_ids.iter().take(FORGET_COUNT) {
        let forget = client
            .forget(*memory_id)
            .mode(ForgetMode::Soft)
            .send()
            .await
            .expect("forget");
        assert_eq!(forget.memory_id, memory_id.raw());
    }

    client.bye().await.expect("bye");
    server.stop().await;

    // ---- 4. Knowledge tables must remain empty ---------------------
    //
    // Open metadata.redb directly (the server's exclusive lock was
    // released on stop) and assert every knowledge-layer table has
    // zero rows.
    let paths = ShardPaths::at(data_dir.path().join("0"));
    let metadata = redb::Database::open(paths.metadata_db()).expect("open metadata.redb");
    let rtxn = metadata.begin_read().expect("read txn");

    assert_table_empty(&rtxn, ENTITIES_TABLE, "entities");
    assert_table_empty(&rtxn, ENTITY_BY_CANONICAL_NAME_TABLE, "entity_by_canonical_name");
    assert_table_empty(&rtxn, ENTITY_ALIASES_TABLE, "entity_aliases");
    assert_table_empty(&rtxn, ENTITY_TRIGRAMS_TABLE, "entity_trigrams");
    assert_table_empty(&rtxn, ENTITY_MENTIONS_TABLE, "entity_mentions");

    assert_table_empty(&rtxn, STATEMENTS_TABLE, "statements");
    assert_table_empty(&rtxn, STATEMENTS_BY_SUBJECT_TABLE, "statements_by_subject");
    assert_table_empty(&rtxn, STATEMENTS_BY_PREDICATE_TABLE, "statements_by_predicate");
    assert_table_empty(
        &rtxn,
        STATEMENTS_BY_OBJECT_ENTITY_TABLE,
        "statements_by_object_entity",
    );
    assert_table_empty(&rtxn, STATEMENTS_BY_EVENT_TIME_TABLE, "statements_by_event_time");
    assert_table_empty(&rtxn, STATEMENTS_BY_EVIDENCE_TABLE, "statements_by_evidence");
    assert_table_empty(&rtxn, STATEMENT_CHAIN_TABLE, "statement_chain");
    assert_table_empty(&rtxn, EVIDENCE_OVERFLOW_TABLE, "evidence_overflow");

    assert_table_empty(&rtxn, RELATIONS_TABLE, "relations");
    assert_table_empty(&rtxn, RELATIONS_BY_FROM_TABLE, "relations_by_from");
    assert_table_empty(&rtxn, RELATIONS_BY_TO_TABLE, "relations_by_to");
    assert_table_empty(&rtxn, RELATIONS_BY_EVIDENCE_TABLE, "relations_by_evidence");

    assert_table_empty(&rtxn, PREDICATES_TABLE, "predicates");
    assert_table_empty(&rtxn, ENTITY_TYPES_TABLE, "entity_types");
    assert_table_empty(&rtxn, RELATION_TYPES_TABLE, "relation_types");
    assert_table_empty(&rtxn, EXTRACTORS_TABLE, "extractors");
    assert_table_empty(&rtxn, SCHEMA_VERSIONS_TABLE, "schema_versions");
    assert_table_empty(&rtxn, EXTRACTOR_AUDIT_TABLE, "extractor_audit");
    assert_table_empty(&rtxn, ENTITY_RESOLUTION_AUDIT_TABLE, "entity_resolution_audit");
    assert_table_empty(&rtxn, MERGE_LOG_TABLE, "merge_log");

    drop(rtxn);
    drop(metadata);

    // ---- 5. WAL must contain zero knowledge frames -----------------
    //
    // Iterate every record and assert `!kind.is_knowledge()`. Substrate
    // records (Encode / Forget / CheckpointBegin / CheckpointEnd / …)
    // are expected; knowledge records (0x10..=0x50) must be zero.
    let shard_uuid = std::fs::read(paths.shard_uuid()).expect("shard.uuid");
    let shard_uuid: [u8; 16] = shard_uuid
        .as_slice()
        .try_into()
        .expect("shard.uuid is 16 bytes");
    let wal_reader =
        brain_storage::wal::reader::WalReader::open(paths.wal_dir(), shard_uuid).expect("wal");

    let mut wal_records_seen: u64 = 0;
    let mut knowledge_records_seen: u64 = 0;
    for item in wal_reader {
        let record = item.expect("wal record decode");
        wal_records_seen += 1;
        if record.kind.is_knowledge() {
            knowledge_records_seen += 1;
        }
    }
    assert!(
        wal_records_seen > 0,
        "expected substrate WAL records to be present; found 0"
    );
    assert_eq!(
        knowledge_records_seen, 0,
        "schema-off run produced {knowledge_records_seen} knowledge WAL records; expected 0"
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
        "knowledge-layer table `{name}` should be empty on schema-off run; found {count} entries"
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
