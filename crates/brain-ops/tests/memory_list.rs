//! Integration tests for `handle_memory_list` (MEMORY_LIST).
//!
//! MEMORY_LIST is a pure paginated enumeration read: it walks the
//! caller's `(namespace, agent)` timeline in a stable order and returns
//! keyset pages. These tests drive the real `dispatch` → handler path —
//! the same code the wire layer calls — over an in-process fixture with a
//! deterministic mock embedder.
//!
//! Coverage: full enumeration across pages (no dup/skip), keyset
//! completeness, stale-cursor rejection on filter change, kind filter,
//! tombstone exclusion, created time-range filter, salience filter,
//! cross-tenant scope isolation, empty result, and last-page cursor.

#![cfg(target_os = "linux")]

use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::{EncodeRequest, ForgetRequest, RequestBody};
use brain_protocol::envelope::response::{EncodeResponse, ResponseBody};
use brain_protocol::{
    ForgetMode, MemoryListDirWire, MemoryListItem, MemoryListRequest, MemoryListResponseFrame,
    MemoryListSortWire, MemoryListTimeAxisWire,
};

// ---------------------------------------------------------------------------
// Mock dispatcher (text-driven deterministic vectors).
// ---------------------------------------------------------------------------

struct MockDispatcher;

impl Dispatcher for MockDispatcher {
    fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        let mut v = [0.0f32; VECTOR_DIM];
        for (i, byte) in text.as_bytes().iter().enumerate() {
            v[i % VECTOR_DIM] += f32::from(*byte) / 255.0;
        }
        Ok(v)
    }

    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        texts.iter().map(|t| self.embed(t)).collect()
    }

    fn fingerprint(&self) -> [u8; 16] {
        [0xAB; 16]
    }
}

// ---------------------------------------------------------------------------
// Fixture.
// ---------------------------------------------------------------------------

struct Fixture {
    ctx: OpsContext,
    _tempdir: tempfile::TempDir,
    metadata: SharedMetadataDb,
}

impl Fixture {
    fn intern_namespace(&self, name: &str) -> brain_core::NamespaceId {
        let wtxn = self.metadata.write_txn().expect("write txn");
        let id = brain_metadata::namespace::namespace_intern_or_get(&wtxn, name, 0)
            .expect("intern namespace");
        wtxn.commit().expect("commit");
        id
    }
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().unwrap();
    let db_path = tempdir.path().join("metadata.redb");
    let metadata: SharedMetadataDb = Arc::new(MetadataDb::open(&db_path).unwrap());

    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).unwrap();
    let writer = Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));

    let embedder: Arc<dyn Dispatcher> = Arc::new(MockDispatcher);
    let executor = ExecutorContext::new(
        embedder,
        shared,
        metadata.clone(),
        writer as Arc<dyn WriterHandle>,
    );

    Fixture {
        ctx: brain_ops::test_support::ops_context_for_tests(executor, tempdir.path()),
        _tempdir: tempdir,
        metadata,
    }
}

/// Two agents inside one namespace so the scope-isolation test proves the
/// agent wall, and one shared namespace so time/kind/salience tests share
/// a tenant.
const AGENT_A: [u8; 16] = [0xA1; 16];
const AGENT_B: [u8; 16] = [0xB2; 16];

fn caller_for(namespace: &str, agent: [u8; 16]) -> RequestCaller {
    let agent = brain_core::AgentId(uuid::Uuid::from_bytes(agent));
    RequestCaller::from_scope(
        agent,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn encode_req(request_id: [u8; 16], text: &str) -> EncodeRequest {
    EncodeRequest {
        text: text.into(),
        context_id: 0,
        request_id,
        txn_id: None,
        occurred_at_unix_nanos: None,
        act_as: None,
        wait: brain_protocol::WaitMode::Ack,
        allow_duplicates: false,
    }
}

fn list_req() -> MemoryListRequest {
    MemoryListRequest {
        sort: MemoryListSortWire::Created,
        dir: MemoryListDirWire::Desc,
        limit: 100,
        cursor: Vec::new(),
        kinds: Vec::new(),
        include_tombstoned: false,
        time_axis: MemoryListTimeAxisWire::Created,
        from_unix_nanos: 0,
        to_unix_nanos: 0,
        salience_min: 0.0,
        salience_max: 1.0,
        text_contains: String::new(),
        act_as: None,
    }
}

async fn encode(fix: &Fixture, caller: RequestCaller, request_id: [u8; 16], text: &str) -> u128 {
    let outcome = dispatch(
        RequestBody::Encode(encode_req(request_id, text)),
        caller,
        &fix.ctx,
    )
    .await
    .expect("encode dispatch");
    match single_body(outcome) {
        ResponseBody::Encode(EncodeResponse { memory_id, .. }) => memory_id,
        other => panic!("expected Encode response, got {other:?}"),
    }
}

async fn forget(fix: &Fixture, caller: RequestCaller, request_id: [u8; 16], memory_id: u128) {
    let outcome = dispatch(
        RequestBody::Forget(ForgetRequest {
            memory_id,
            mode: ForgetMode::Soft,
            request_id,
            txn_id: None,
            act_as: None,
        }),
        caller,
        &fix.ctx,
    )
    .await
    .expect("forget dispatch");
    match single_body(outcome) {
        ResponseBody::Forget(_) => {}
        other => panic!("expected Forget response, got {other:?}"),
    }
}

async fn list(
    fix: &Fixture,
    caller: RequestCaller,
    req: MemoryListRequest,
) -> MemoryListResponseFrame {
    let outcome = dispatch(RequestBody::MemoryList(req), caller, &fix.ctx)
        .await
        .expect("memory_list dispatch");
    match single_body(outcome) {
        ResponseBody::MemoryList(f) => f,
        other => panic!("expected MemoryList response, got {other:?}"),
    }
}

/// `memory_id` bytes → the u128 wire id used by ENCODE responses.
fn item_id(item: &MemoryListItem) -> u128 {
    u128::from_be_bytes(item.memory_id)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// Full enumeration across keyset pages returns every memory exactly once,
/// with no duplicates and no skips, and the last page carries an empty
/// cursor.
#[test]
fn paginates_full_corpus_without_dup_or_skip() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let caller = || caller_for("acme", AGENT_A);

        const N: usize = 25;
        let mut encoded = Vec::new();
        for i in 0..N {
            let id = encode(
                &fix,
                caller(),
                [i as u8 + 1; 16],
                &format!("memory number {i}"),
            )
            .await;
            encoded.push(id);
        }

        // Page through with a small limit to force multiple keyset seeks.
        let mut seen: Vec<u128> = Vec::new();
        let mut cursor = Vec::new();
        let mut pages = 0;
        loop {
            let mut req = list_req();
            req.limit = 7;
            req.cursor = cursor.clone();
            let frame = list(&fix, caller(), req).await;
            assert!(frame.is_final, "each MEMORY_LIST frame is final in v1");
            assert!(frame.items.len() <= 7, "page must not exceed limit");
            for it in &frame.items {
                seen.push(item_id(it));
            }
            pages += 1;
            if frame.next_cursor.is_empty() {
                break;
            }
            cursor = frame.next_cursor;
            assert!(pages < 100, "pagination must terminate");
        }

        assert!(
            pages >= 4,
            "25 items at limit 7 should span >=4 pages, got {pages}"
        );

        // No duplicates.
        let mut dedup = seen.clone();
        dedup.sort_unstable();
        dedup.dedup();
        assert_eq!(
            dedup.len(),
            seen.len(),
            "no memory_id may repeat across pages"
        );

        // Exact set equality with what we encoded (no skips).
        let mut want = encoded.clone();
        want.sort_unstable();
        assert_eq!(
            dedup, want,
            "enumeration must return exactly the encoded set"
        );
    })
}

/// Changing a filter between the cursor mint and the resume request is
/// rejected as a stale cursor — the resumed page would otherwise belong to
/// a different result set.
#[test]
fn stale_cursor_rejected_on_filter_change() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let caller = || caller_for("acme", AGENT_A);

        for i in 0..10 {
            encode(&fix, caller(), [i as u8 + 1; 16], &format!("item {i}")).await;
        }

        let mut req = list_req();
        req.limit = 3;
        let frame = list(&fix, caller(), req).await;
        assert!(!frame.next_cursor.is_empty(), "expected a non-empty cursor");
        let cursor = frame.next_cursor;

        // Resume with a DIFFERENT filter (salience floor changed) → stale.
        let mut changed = list_req();
        changed.limit = 3;
        changed.cursor = cursor.clone();
        changed.salience_min = 0.4;
        let outcome = dispatch(RequestBody::MemoryList(changed), caller(), &fix.ctx).await;
        let err = outcome.expect_err("filter change must reject the cursor");
        assert!(
            format!("{err}").contains("stale_cursor"),
            "expected stale_cursor error, got {err}"
        );

        // Same filter resumes fine.
        let mut same = list_req();
        same.limit = 3;
        same.cursor = cursor;
        let _ = list(&fix, caller(), same).await;
    })
}

/// The kind filter narrows to the requested kinds; a kind that no memory
/// has yields an empty page.
#[test]
fn kind_filter_narrows_results() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let caller = || caller_for("acme", AGENT_A);

        for i in 0..5 {
            encode(&fix, caller(), [i as u8 + 1; 16], &format!("fact {i}")).await;
        }

        // Discover the kinds actually assigned by the router.
        let all = list(&fix, caller(), list_req()).await;
        assert_eq!(all.items.len(), 5);
        let present: std::collections::BTreeSet<u8> = all.items.iter().map(|i| i.kind).collect();

        // Filtering to every present kind returns everything.
        let mut req = list_req();
        req.kinds = present
            .iter()
            .map(|&k| match k {
                0 => brain_protocol::MemoryKindWire::Episodic,
                1 => brain_protocol::MemoryKindWire::Semantic,
                _ => brain_protocol::MemoryKindWire::Consolidated,
            })
            .collect();
        let filtered = list(&fix, caller(), req).await;
        assert_eq!(filtered.items.len(), 5, "all present kinds return all rows");

        // Filtering to an absent kind returns nothing.
        let absent = [
            brain_protocol::MemoryKindWire::Episodic,
            brain_protocol::MemoryKindWire::Semantic,
            brain_protocol::MemoryKindWire::Consolidated,
        ]
        .into_iter()
        .find(|k| !present.contains(&(*k as u8)));
        if let Some(k) = absent {
            let mut req = list_req();
            req.kinds = vec![k];
            let none = list(&fix, caller(), req).await;
            assert!(none.items.is_empty(), "absent-kind filter must be empty");
        }
    })
}

/// A tombstoned memory is excluded from the default enumeration.
#[test]
fn tombstoned_memory_excluded() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let caller = || caller_for("acme", AGENT_A);

        let keep = encode(&fix, caller(), [1; 16], "keep this memory").await;
        let drop = encode(&fix, caller(), [2; 16], "forget this memory").await;

        forget(&fix, caller(), [9; 16], drop).await;

        let frame = list(&fix, caller(), list_req()).await;
        let ids: Vec<u128> = frame.items.iter().map(item_id).collect();
        assert!(ids.contains(&keep), "surviving memory must be listed");
        assert!(
            !ids.contains(&drop),
            "tombstoned memory must not be enumerated"
        );

        // include_tombstoned is accepted and still returns the active row.
        // (v1 timeline index drops tombstoned rows, so the forgotten memory
        // stays absent — see handler docs.)
        let mut req = list_req();
        req.include_tombstoned = true;
        let frame = list(&fix, caller(), req).await;
        let ids: Vec<u128> = frame.items.iter().map(item_id).collect();
        assert!(ids.contains(&keep));
        assert!(!ids.contains(&drop));
    })
}

/// The created time-range filter admits only rows whose `created_at` falls
/// in `[from, to]`.
#[test]
fn created_time_range_filter() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let caller = || caller_for("acme", AGENT_A);

        for i in 0..6 {
            encode(&fix, caller(), [i as u8 + 1; 16], &format!("dated {i}")).await;
        }

        // Ascending so we can pick a middle window by created_at.
        let mut all_req = list_req();
        all_req.dir = MemoryListDirWire::Asc;
        let all = list(&fix, caller(), all_req).await;
        assert_eq!(all.items.len(), 6);
        let times: Vec<u64> = all.items.iter().map(|i| i.created_at_unix_nanos).collect();

        // Window bounding the middle rows (indices 1..=4 inclusive).
        let from = times[1];
        let to = times[4];
        let mut req = list_req();
        req.dir = MemoryListDirWire::Asc;
        req.from_unix_nanos = from;
        req.to_unix_nanos = to;
        let frame = list(&fix, caller(), req).await;
        for it in &frame.items {
            assert!(
                it.created_at_unix_nanos >= from && it.created_at_unix_nanos <= to,
                "row {} outside [{from}, {to}]",
                it.created_at_unix_nanos
            );
        }
        // Every row in the window must be present.
        let want: Vec<u128> = all.items[1..=4].iter().map(item_id).collect();
        let got: std::collections::BTreeSet<u128> = frame.items.iter().map(item_id).collect();
        for id in want {
            assert!(got.contains(&id), "windowed row {id} missing");
        }
    })
}

/// The salience-range filter excludes rows outside `[min, max]`. ENCODE
/// assigns the router default salience, so a floor above it empties the
/// page and a full range returns everything.
#[test]
fn salience_range_filter() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");
        let caller = || caller_for("acme", AGENT_A);

        for i in 0..4 {
            encode(&fix, caller(), [i as u8 + 1; 16], &format!("sal {i}")).await;
        }

        let all = list(&fix, caller(), list_req()).await;
        assert_eq!(all.items.len(), 4);
        let sal = all.items[0].salience;

        // A floor just above the assigned salience empties the result.
        let mut high = list_req();
        high.salience_min = (sal + 0.1).min(1.0);
        let none = list(&fix, caller(), high).await;
        assert!(
            none.items.is_empty(),
            "floor above salience {sal} must be empty"
        );

        // A ceiling just below empties it too.
        let mut low = list_req();
        low.salience_max = (sal - 0.1).max(0.0);
        let none = list(&fix, caller(), low).await;
        assert!(
            none.items.is_empty(),
            "ceiling below salience must be empty"
        );
    })
}

/// A second agent's memories are never returned to the first agent, even
/// inside the same namespace.
#[test]
fn scope_isolation_between_agents() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");

        let a_mem = encode(
            &fix,
            caller_for("acme", AGENT_A),
            [1; 16],
            "agent A private",
        )
        .await;
        let b_mem = encode(
            &fix,
            caller_for("acme", AGENT_B),
            [2; 16],
            "agent B private",
        )
        .await;
        assert_ne!(a_mem, b_mem);

        let a_view = list(&fix, caller_for("acme", AGENT_A), list_req()).await;
        let a_ids: Vec<u128> = a_view.items.iter().map(item_id).collect();
        assert!(a_ids.contains(&a_mem), "A must see its own memory");
        assert!(
            !a_ids.contains(&b_mem),
            "SCOPE BREACH: A's list returned B's memory"
        );

        let b_view = list(&fix, caller_for("acme", AGENT_B), list_req()).await;
        let b_ids: Vec<u128> = b_view.items.iter().map(item_id).collect();
        assert!(b_ids.contains(&b_mem), "B must see its own memory");
        assert!(
            !b_ids.contains(&a_mem),
            "SCOPE BREACH: B's list returned A's memory"
        );
    })
}

/// Enumerating an agent with no memories yields an empty page with an
/// empty cursor.
#[test]
fn empty_result_when_no_memories() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");

        let frame = list(&fix, caller_for("acme", AGENT_A), list_req()).await;
        assert!(frame.items.is_empty(), "no memories → empty page");
        assert!(
            frame.next_cursor.is_empty(),
            "empty page → exhausted cursor"
        );
        assert_eq!(frame.cumulative_count, 0);
        assert!(frame.is_final);
    })
}

/// Rejects an out-of-range limit.
#[test]
fn rejects_bad_limit() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _ns = fix.intern_namespace("acme");

        let mut req = list_req();
        req.limit = 0;
        let err = dispatch(
            RequestBody::MemoryList(req),
            caller_for("acme", AGENT_A),
            &fix.ctx,
        )
        .await
        .expect_err("limit 0 must be rejected");
        assert!(format!("{err}").contains("limit"));

        let mut req = list_req();
        req.limit = 101;
        let err = dispatch(
            RequestBody::MemoryList(req),
            caller_for("acme", AGENT_A),
            &fix.ctx,
        )
        .await
        .expect_err("limit 101 must be rejected");
        assert!(format!("{err}").contains("limit"));
    })
}
