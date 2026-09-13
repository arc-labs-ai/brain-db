//! Multi-frame cursor pagination for the four typed-graph list read ops
//! (`ENTITY_LIST` / `STATEMENT_LIST` / `RELATION_LIST_FROM` / `_TO`).
//!
//! Each op inserts more rows than the page limit, then walks every page
//! by feeding the previous response's `next_cursor` back in. The proof for
//! all four is identical:
//!  a. every row appears exactly once across the pages (no duplicate),
//!  b. no overlaps and no gaps versus the full single-page snapshot,
//!  c. `next_cursor` is empty only on the final page,
//!  d. a malformed cursor is rejected with an error, never a panic.
//!
//! Drives the real `dispatch` → handler path, the same code the wire layer
//! calls, with a strict-mode `RequestCaller` bound to one interned
//! namespace + space (mirrors `typed_graph_namespace_isolation.rs`).

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::sync::Arc;

use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{
    EntityCreateRequest, EntityCreateResponse, EntityListRequest, EntityListResponseFrame,
    EvidenceRefWire, RelationCreateRequest, RelationCreateResponse, RelationListFromRequest,
    RelationListFromResponseFrame, RelationListToRequest, RelationListToResponseFrame,
    StatementCreateRequest, StatementCreateResponse, StatementKindWire, StatementListRequest,
    StatementListResponseFrame, StatementObjectWire, StatementValueWire,
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

    let ctx = brain_ops::test_support::ops_context_for_tests(executor, tempdir.path());
    std::mem::forget(tempdir);
    Fixture { ctx, metadata }
}

const SPACE: [u8; 16] = [0xC1; 16];

fn caller(namespace: &str) -> RequestCaller {
    let space = brain_core::SpaceId(uuid::Uuid::from_bytes(SPACE));
    RequestCaller::from_scope(
        space,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn req_id(n: u32) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..4].copy_from_slice(&n.to_le_bytes());
    b
}

// ---------------------------------------------------------------------------
// Wire op drivers.
// ---------------------------------------------------------------------------

async fn create_entity(fix: &Fixture, c: RequestCaller, req: u32, canonical: &str) -> [u8; 16] {
    let request = EntityCreateRequest {
        entity_type_id: 1, // brain:Person, seeded at id = 1.
        canonical_name: canonical.to_string(),
        aliases: Vec::new(),
        attributes_blob: Vec::new(),
        session_id: 0,
        request_id: req_id(req),
        act_as: None,
    };
    let outcome = dispatch(RequestBody::EntityCreate(request), c, &fix.ctx)
        .await
        .expect("entity_create dispatch");
    match single_body(outcome) {
        ResponseBody::EntityCreate(EntityCreateResponse { entity_id }) => entity_id,
        other => panic!("expected EntityCreate, got {other:?}"),
    }
}

async fn create_statement(
    fix: &Fixture,
    c: RequestCaller,
    req: u32,
    subject: [u8; 16],
    predicate: &str,
    value: &str,
) -> [u8; 16] {
    let request = StatementCreateRequest {
        kind: StatementKindWire::Fact,
        subject,
        predicate: predicate.to_string(),
        object: StatementObjectWire::Value(StatementValueWire::Text(value.to_string())),
        confidence: 0.95,
        evidence: EvidenceRefWire::Inline(Vec::new()),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 0,
        session_id: 0,
        request_id: req_id(req),
        act_as: None,
    };
    let outcome = dispatch(RequestBody::StatementCreate(request), c, &fix.ctx)
        .await
        .expect("statement_create dispatch");
    match single_body(outcome) {
        ResponseBody::StatementCreate(StatementCreateResponse { statement_id, .. }) => statement_id,
        other => panic!("expected StatementCreate, got {other:?}"),
    }
}

async fn create_relation(
    fix: &Fixture,
    c: RequestCaller,
    req: u32,
    from: [u8; 16],
    to: [u8; 16],
    relation_type: &str,
) -> [u8; 16] {
    let request = RelationCreateRequest {
        relation_type: relation_type.to_string(),
        from_entity: from,
        to_entity: to,
        properties_blob: Vec::new(),
        evidence: EvidenceRefWire::Inline(Vec::new()),
        extractor_id: 0,
        confidence: 0.95,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        session_id: 0,
        request_id: req_id(req),
        act_as: None,
    };
    let outcome = dispatch(RequestBody::RelationCreate(request), c, &fix.ctx)
        .await
        .expect("relation_create dispatch");
    match single_body(outcome) {
        ResponseBody::RelationCreate(RelationCreateResponse { relation_id }) => relation_id,
        other => panic!("expected RelationCreate, got {other:?}"),
    }
}

// --- list one page ---

async fn entity_page(fix: &Fixture, c: RequestCaller, limit: u32, cursor: Vec<u8>) -> Page {
    let req = EntityListRequest {
        entity_type_id: 1,
        name_prefix: String::new(),
        mention_count_min: 0,
        include_tombstoned: false,
        include_merged: false,
        limit,
        cursor,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::EntityList(req), c, &fix.ctx)
        .await
        .expect("entity_list dispatch");
    match single_body(outcome) {
        ResponseBody::EntityList(EntityListResponseFrame {
            items, next_cursor, ..
        }) => Page {
            ids: items.into_iter().map(|i| i.entity.entity_id).collect(),
            next_cursor,
        },
        other => panic!("expected EntityList, got {other:?}"),
    }
}

async fn statement_page(
    fix: &Fixture,
    c: RequestCaller,
    subject: [u8; 16],
    limit: u32,
    cursor: Vec<u8>,
) -> Page {
    let req = StatementListRequest {
        subject,
        predicate: String::new(),
        kind: 0,
        min_confidence: 0.0,
        time_range_start_unix_nanos: 0,
        time_range_end_unix_nanos: 0,
        only_current: true,
        include_tombstoned: false,
        limit,
        cursor,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::StatementList(req), c, &fix.ctx)
        .await
        .expect("statement_list dispatch");
    match single_body(outcome) {
        ResponseBody::StatementList(StatementListResponseFrame {
            items, next_cursor, ..
        }) => Page {
            ids: items.into_iter().map(|s| s.statement_id).collect(),
            next_cursor,
        },
        other => panic!("expected StatementList, got {other:?}"),
    }
}

async fn relation_from_page(
    fix: &Fixture,
    c: RequestCaller,
    from: [u8; 16],
    limit: u32,
    cursor: Vec<u8>,
) -> Page {
    let req = RelationListFromRequest {
        from_entity: from,
        relation_type_filter: String::new(),
        time_range_start_unix_nanos: 0,
        time_range_end_unix_nanos: 0,
        include_superseded: false,
        include_tombstoned: false,
        limit,
        cursor,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::RelationListFrom(req), c, &fix.ctx)
        .await
        .expect("relation_list_from dispatch");
    match single_body(outcome) {
        ResponseBody::RelationListFrom(RelationListFromResponseFrame {
            items,
            next_cursor,
            ..
        }) => Page {
            ids: items.into_iter().map(|r| r.relation_id).collect(),
            next_cursor,
        },
        other => panic!("expected RelationListFrom, got {other:?}"),
    }
}

async fn relation_to_page(
    fix: &Fixture,
    c: RequestCaller,
    to: [u8; 16],
    limit: u32,
    cursor: Vec<u8>,
) -> Page {
    let req = RelationListToRequest {
        to_entity: to,
        relation_type_filter: String::new(),
        time_range_start_unix_nanos: 0,
        time_range_end_unix_nanos: 0,
        include_superseded: false,
        include_tombstoned: false,
        limit,
        cursor,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::RelationListTo(req), c, &fix.ctx)
        .await
        .expect("relation_list_to dispatch");
    match single_body(outcome) {
        ResponseBody::RelationListTo(RelationListToResponseFrame {
            items, next_cursor, ..
        }) => Page {
            ids: items.into_iter().map(|r| r.relation_id).collect(),
            next_cursor,
        },
        other => panic!("expected RelationListTo, got {other:?}"),
    }
}

struct Page {
    ids: Vec<[u8; 16]>,
    next_cursor: Vec<u8>,
}

/// Assert the walked pages tile `expected` exactly: same set, no
/// duplicates, and only the last page carries an empty cursor.
fn assert_tiles(pages: &[Vec<[u8; 16]>], limit: usize, expected: &BTreeSet<[u8; 16]>) {
    let mut seen: Vec<[u8; 16]> = Vec::new();
    for (i, page) in pages.iter().enumerate() {
        let is_last = i + 1 == pages.len();
        if !is_last {
            assert_eq!(page.len(), limit, "non-final page must be full");
        }
        assert!(!page.is_empty(), "no empty pages before exhaustion");
        seen.extend(page.iter().copied());
    }
    // (a) exactly once each — no duplicate across pages.
    let unique: BTreeSet<[u8; 16]> = seen.iter().copied().collect();
    assert_eq!(unique.len(), seen.len(), "a row appeared on two pages");
    // (b) no gaps / no overlaps versus the full expected set.
    assert_eq!(&unique, expected, "pages did not tile the full set");
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

const LIMIT: u32 = 3;
const N: u32 = 7; // 7 rows over a limit of 3 → pages of 3, 3, 1.

#[test]
fn entity_list_pages_cover_every_row_once() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        let c = || caller("acme");

        let mut expected = BTreeSet::new();
        for i in 0..N {
            let id = create_entity(&fix, c(), i, &format!("Person {i}")).await;
            expected.insert(id);
        }

        let mut pages: Vec<Vec<[u8; 16]>> = Vec::new();
        let mut cursor = Vec::new();
        loop {
            let page = entity_page(&fix, c(), LIMIT, cursor).await;
            let next = page.next_cursor.clone();
            pages.push(page.ids);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        assert_tiles(&pages, LIMIT as usize, &expected);
        assert!(
            pages.last().unwrap().len() < LIMIT as usize
                || pages.len() * (LIMIT as usize) == N as usize,
            "final page short",
        );

        // (d) a malformed cursor is rejected, not paniced through.
        let bad = EntityListRequest {
            entity_type_id: 1,
            name_prefix: String::new(),
            mention_count_min: 0,
            include_tombstoned: false,
            include_merged: false,
            limit: LIMIT,
            cursor: vec![9, 9, 9],
            act_as: None,
        };
        assert!(dispatch(RequestBody::EntityList(bad), c(), &fix.ctx)
            .await
            .is_err());
    });
}

#[test]
fn statement_list_pages_cover_every_row_once() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        let c = || caller("acme");

        let subject = create_entity(&fix, c(), 1000, "Subject").await;
        let mut expected = BTreeSet::new();
        for i in 0..N {
            // Distinct predicate per statement so each stays a separate
            // current Fact (no supersession / consolidation collapse).
            let id = create_statement(
                &fix,
                c(),
                i,
                subject,
                &format!("acme:attr{i}"),
                &format!("value {i}"),
            )
            .await;
            expected.insert(id);
        }

        let mut pages: Vec<Vec<[u8; 16]>> = Vec::new();
        let mut cursor = Vec::new();
        loop {
            let page = statement_page(&fix, c(), subject, LIMIT, cursor).await;
            let next = page.next_cursor.clone();
            pages.push(page.ids);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        assert_tiles(&pages, LIMIT as usize, &expected);

        let bad = StatementListRequest {
            subject,
            predicate: String::new(),
            kind: 0,
            min_confidence: 0.0,
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            only_current: true,
            include_tombstoned: false,
            limit: LIMIT,
            cursor: vec![1, 2, 3, 4],
            act_as: None,
        };
        assert!(dispatch(RequestBody::StatementList(bad), c(), &fix.ctx)
            .await
            .is_err());
    });
}

/// Regression: keyset pagination must reach rows past the former
/// 1000-row in-memory window. The old handler fetched a fixed
/// `LIST_LIMIT_MAX`-row window from the store and paginated it in memory,
/// so any row beyond 1000 was unreachable and every page re-fetched +
/// re-sorted the whole window. Insert > 1000 statements under one subject
/// and prove every one is emitted, exactly once, across the full walk.
#[test]
fn statement_list_pages_past_one_thousand() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        let c = || caller("acme");

        let subject = create_entity(&fix, c(), 1_000_000, "BigSubject").await;
        const BIG: u32 = 1050; // strictly past the 1000-row ceiling.
        let mut expected = BTreeSet::new();
        for i in 0..BIG {
            let id = create_statement(
                &fix,
                c(),
                i,
                subject,
                &format!("acme:attr{i}"),
                &format!("value {i}"),
            )
            .await;
            expected.insert(id);
        }
        assert_eq!(expected.len(), BIG as usize, "creates must be distinct");

        let page_limit: u32 = 250;
        let mut pages: Vec<Vec<[u8; 16]>> = Vec::new();
        let mut cursor = Vec::new();
        loop {
            let page = statement_page(&fix, c(), subject, page_limit, cursor).await;
            let next = page.next_cursor.clone();
            pages.push(page.ids);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        assert_tiles(&pages, page_limit as usize, &expected);
        // Must have needed more than four full pages of 250 — i.e. we
        // genuinely paged past 1000.
        let total: usize = pages.iter().map(Vec::len).sum();
        assert_eq!(total, BIG as usize, "every row past 1000 reachable");
        assert!(pages.len() >= 5, "should span > 1000 rows across pages");
    });
}

/// Same reachability guarantee for the relation edge-index walk.
#[test]
fn relation_list_from_pages_past_one_thousand() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        let c = || caller("acme");

        let from = create_entity(&fix, c(), 1_000_000, "BigFrom").await;
        const BIG: u32 = 1050;
        let mut expected = BTreeSet::new();
        for i in 0..BIG {
            let to = create_entity(&fix, c(), 2_000_000 + i, &format!("To {i}")).await;
            let id = create_relation(&fix, c(), i, from, to, "acme:knows").await;
            expected.insert(id);
        }
        assert_eq!(expected.len(), BIG as usize, "creates must be distinct");

        let page_limit: u32 = 250;
        let mut pages: Vec<Vec<[u8; 16]>> = Vec::new();
        let mut cursor = Vec::new();
        loop {
            let page = relation_from_page(&fix, c(), from, page_limit, cursor).await;
            let next = page.next_cursor.clone();
            pages.push(page.ids);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        assert_tiles(&pages, page_limit as usize, &expected);
        let total: usize = pages.iter().map(Vec::len).sum();
        assert_eq!(total, BIG as usize, "every relation past 1000 reachable");
        assert!(pages.len() >= 5, "should span > 1000 rows across pages");
    });
}

#[test]
fn relation_list_from_pages_cover_every_row_once() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        let c = || caller("acme");

        let from = create_entity(&fix, c(), 1000, "From").await;
        let mut expected = BTreeSet::new();
        for i in 0..N {
            let to = create_entity(&fix, c(), 2000 + i, &format!("To {i}")).await;
            let id = create_relation(&fix, c(), i, from, to, "acme:knows").await;
            expected.insert(id);
        }

        let mut pages: Vec<Vec<[u8; 16]>> = Vec::new();
        let mut cursor = Vec::new();
        loop {
            let page = relation_from_page(&fix, c(), from, LIMIT, cursor).await;
            let next = page.next_cursor.clone();
            pages.push(page.ids);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        assert_tiles(&pages, LIMIT as usize, &expected);

        let bad = RelationListFromRequest {
            from_entity: from,
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: LIMIT,
            cursor: vec![0xFF; 8],
            act_as: None,
        };
        assert!(dispatch(RequestBody::RelationListFrom(bad), c(), &fix.ctx)
            .await
            .is_err());
    });
}

#[test]
fn relation_list_to_pages_cover_every_row_once() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        let c = || caller("acme");

        let to = create_entity(&fix, c(), 1000, "To").await;
        let mut expected = BTreeSet::new();
        for i in 0..N {
            let from = create_entity(&fix, c(), 2000 + i, &format!("From {i}")).await;
            let id = create_relation(&fix, c(), i, from, to, "acme:knows").await;
            expected.insert(id);
        }

        let mut pages: Vec<Vec<[u8; 16]>> = Vec::new();
        let mut cursor = Vec::new();
        loop {
            let page = relation_to_page(&fix, c(), to, LIMIT, cursor).await;
            let next = page.next_cursor.clone();
            pages.push(page.ids);
            if next.is_empty() {
                break;
            }
            cursor = next;
        }
        assert_tiles(&pages, LIMIT as usize, &expected);

        let bad = RelationListToRequest {
            to_entity: to,
            relation_type_filter: String::new(),
            time_range_start_unix_nanos: 0,
            time_range_end_unix_nanos: 0,
            include_superseded: false,
            include_tombstoned: false,
            limit: LIMIT,
            cursor: vec![7; 40],
            act_as: None,
        };
        assert!(dispatch(RequestBody::RelationListTo(bad), c(), &fix.ctx)
            .await
            .is_err());
    });
}
