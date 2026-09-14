//! `GRAPH_FETCH` — tenant isolation of the edge-expansion walk.
//!
//! The export seeds from the caller's scope-bounded statement spine, but the
//! edge expansion walks the shared, un-prefixed edge tables. The write path
//! validates endpoint EXISTENCE, not SCOPE, so tenant B can create a relation
//! whose far endpoint is one of tenant A's entities. When A runs GRAPH_FETCH,
//! that foreign relation is incident to A's seed entity and would surface B's
//! entity name + edge unless every emitted node/edge is re-checked against the
//! caller's `(namespace, space)`.
//!
//! This pins the wall: A's own graph comes back complete, and NOTHING B owns
//! (entity name, relation edge, or the foreign entity id) appears in A's page.

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
    EntityCreateRequest, EntityCreateResponse, EvidenceRefWire, GraphFetchRequest,
    GraphFetchResponseFrame, RelationCreateRequest, RelationCreateResponse, StatementCreateRequest,
    StatementCreateResponse, StatementKindWire, StatementObjectWire, StatementValueWire,
};

// ---------------------------------------------------------------------------
// Fixture (text-driven deterministic vectors; same shape as the sibling proofs).
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

struct Fixture {
    ctx: OpsContext,
    metadata: SharedMetadataDb,
}

impl Fixture {
    fn intern_namespace(&self, name: &str) {
        let wtxn = self.metadata.write_txn().expect("write txn");
        brain_metadata::namespace::namespace_intern_or_get(&wtxn, name, 0).expect("intern");
        wtxn.commit().expect("commit");
    }
}

fn build_fixture() -> Fixture {
    let tempdir = tempfile::tempdir().expect("tempdir");
    let metadata: SharedMetadataDb =
        Arc::new(MetadataDb::open(tempdir.path().join("metadata.redb")).expect("open metadata"));
    let (shared, hnsw_writer) = SharedHnsw::new(IndexParams::default_v1()).expect("hnsw");
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

// A owns `acme/chatbot`; B owns `evil/bot`. Distinct namespace AND space.
const ACME_SPACE: [u8; 16] = [0xA1; 16];
const EVIL_SPACE: [u8; 16] = [0xE7; 16];

fn caller(namespace: &str, space_bytes: [u8; 16]) -> RequestCaller {
    let space = brain_core::SpaceId(uuid::Uuid::from_bytes(space_bytes));
    RequestCaller::from_scope(
        space,
        [0u8; 16],
        [0u8; 16],
        namespace.to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

// ---------------------------------------------------------------------------
// Wire op drivers.
// ---------------------------------------------------------------------------

async fn create_entity(
    fix: &Fixture,
    c: RequestCaller,
    request_id: [u8; 16],
    canonical: &str,
) -> [u8; 16] {
    let req = EntityCreateRequest {
        entity_type_id: 1, // brain:Person, seeded at id = 1.
        canonical_name: canonical.to_string(),
        aliases: Vec::new(),
        attributes_blob: Vec::new(),
        session_id: 0,
        request_id,
        act_as: None,
    };
    match single_body(
        dispatch(RequestBody::EntityCreate(req), c, &fix.ctx)
            .await
            .expect("entity_create"),
    ) {
        ResponseBody::EntityCreate(EntityCreateResponse { entity_id }) => entity_id,
        other => panic!("expected EntityCreate, got {other:?}"),
    }
}

async fn create_statement(
    fix: &Fixture,
    c: RequestCaller,
    request_id: [u8; 16],
    subject: [u8; 16],
    value: &str,
) {
    let req = StatementCreateRequest {
        kind: StatementKindWire::Fact,
        subject,
        predicate: "app:role".to_string(),
        object: StatementObjectWire::Value(StatementValueWire::Text(value.to_string())),
        confidence: 0.95,
        evidence: EvidenceRefWire::Inline(Vec::new()),
        extractor_id: 0,
        valid_from_unix_nanos: 0,
        valid_to_unix_nanos: 0,
        event_at_unix_nanos: 0,
        schema_version: 0,
        session_id: 0,
        request_id,
        act_as: None,
    };
    match single_body(
        dispatch(RequestBody::StatementCreate(req), c, &fix.ctx)
            .await
            .expect("statement_create"),
    ) {
        ResponseBody::StatementCreate(StatementCreateResponse { .. }) => {}
        other => panic!("expected StatementCreate, got {other:?}"),
    }
}

async fn create_relation(
    fix: &Fixture,
    c: RequestCaller,
    request_id: [u8; 16],
    from: [u8; 16],
    to: [u8; 16],
    relation_type: &str,
) -> [u8; 16] {
    let req = RelationCreateRequest {
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
        request_id,
        act_as: None,
    };
    match single_body(
        dispatch(RequestBody::RelationCreate(req), c, &fix.ctx)
            .await
            .expect("relation_create"),
    ) {
        ResponseBody::RelationCreate(RelationCreateResponse { relation_id }) => relation_id,
        other => panic!("expected RelationCreate, got {other:?}"),
    }
}

fn fetch_request() -> GraphFetchRequest {
    GraphFetchRequest {
        limit: 500,
        cursor: Vec::new(),
        include_statements: true,
        include_memories: false,
        include_memory_edges: false,
        include_tombstoned: false,
        act_as: None,
    }
}

async fn fetch(fix: &Fixture, c: RequestCaller) -> GraphFetchResponseFrame {
    match single_body(
        dispatch(RequestBody::GraphFetch(fetch_request()), c, &fix.ctx)
            .await
            .expect("graph_fetch"),
    ) {
        ResponseBody::GraphFetch(f) => f,
        other => panic!("expected GraphFetch, got {other:?}"),
    }
}

fn node_ids(frame: &GraphFetchResponseFrame) -> BTreeSet<[u8; 16]> {
    frame.nodes.iter().map(|n| n.id).collect()
}

// ---------------------------------------------------------------------------
// The proof.
// ---------------------------------------------------------------------------

/// Tenant B creates an entity and a relation whose far endpoint is one of
/// tenant A's entities. A's GRAPH_FETCH must return A's own graph in full and
/// must NOT surface B's entity (id or canonical name) or B's cross-tenant edge.
#[test]
fn graph_fetch_edge_expansion_excludes_foreign_tenant() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        fix.intern_namespace("acme");
        fix.intern_namespace("evil");
        let a = caller("acme", ACME_SPACE);
        let b = caller("evil", EVIL_SPACE);

        // Tenant A's own graph: Alpha (a statement subject → on the spine),
        // AlphaFriend, and a relation Alpha → AlphaFriend that MUST come back.
        let alpha = create_entity(&fix, a.clone(), [1; 16], "Alpha Public").await;
        let friend = create_entity(&fix, a.clone(), [2; 16], "Alpha Friend").await;
        create_statement(&fix, a.clone(), [3; 16], alpha, "lead").await;
        let own_rel = create_relation(&fix, a.clone(), [4; 16], alpha, friend, "app:knows").await;

        // Tenant B's entity + a relation incident to A's Alpha. The write path
        // checks endpoint existence, not scope, so this is accepted — the edge
        // now sits in the shared edge table pointing at A's seed entity.
        let beta = create_entity(&fix, b.clone(), [5; 16], "BetaSecret Confidential").await;
        let foreign_rel = create_relation(&fix, b.clone(), [6; 16], beta, alpha, "app:owns").await;

        let page = fetch(&fix, a).await;
        let ids = node_ids(&page);

        // A's own graph is fully present.
        assert!(ids.contains(&alpha), "A's Alpha must be on the page");
        assert!(ids.contains(&friend), "A's AlphaFriend must be on the page");
        assert!(
            page.edges
                .iter()
                .any(|e| e.from_id == alpha && e.to_id == friend),
            "A's own Alpha→AlphaFriend relation must be returned; got {:?}",
            page.edges
        );

        // Nothing B owns leaks.
        assert!(
            !ids.contains(&beta),
            "TENANT BREACH: B's entity id surfaced in A's GRAPH_FETCH"
        );
        assert!(
            !page
                .nodes
                .iter()
                .any(|n| n.label == "BetaSecret Confidential"),
            "TENANT BREACH: B's entity canonical name surfaced in A's export"
        );
        assert!(
            !page
                .edges
                .iter()
                .any(|e| e.from_id == beta || e.to_id == beta),
            "TENANT BREACH: B's cross-tenant edge surfaced in A's export: {:?}",
            page.edges
        );

        // The two relations are distinct ids (sanity: the foreign one exists,
        // it is simply walled out of A's export rather than never written).
        assert_ne!(own_rel, foreign_rel);
    })
}

/// A hand-forged GRAPH_FETCH cursor: correct version + flags for
/// [`fetch_request`] (include_statements only), but an embedded stmt-key that
/// names the reserved `ns=0/space=0` floor instead of the caller's scope.
///
/// Wire layout (opaque bytes on the wire): `[version(1)][flags(1)]` then the
/// serialized `STATEMENTS_BY_SUBJECT` key `ns(4)+space(16)+subject(16)+
/// kind(1)+predicate(4)+is_current(1)+statement_id(16)` = 58 bytes. All key
/// bytes left zero → the floor.
fn forged_zero_scope_cursor() -> Vec<u8> {
    const FLAG_STATEMENTS: u8 = 0b0001;
    let mut cur = vec![0u8; 60];
    cur[0] = 1; // CURSOR_VERSION
    cur[1] = FLAG_STATEMENTS; // matches fetch_request()'s flags
    cur
}

/// Regression for the cross-tenant leak: before the cursor was bound to the
/// caller's tenant, `decode_cursor` accepted any well-formed cursor and used
/// its embedded key as the scan lower bound. A forged cursor claiming the
/// reserved `ns=0/space=0` floor widened the range to every tenant sorting
/// below the caller and emitted their statement text/values.
///
/// Here B's namespace is interned first so it takes the lower id and sorts
/// beneath A on the shared statement index — exactly the rows a `(0,0)` floor
/// would sweep in. The forged cursor MUST be rejected, and A's clean page
/// MUST never contain B's secret statement value.
#[test]
fn graph_fetch_forged_zero_scope_cursor_rejected_and_no_leak() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        // Intern B first → lower namespace id → sorts below A.
        fix.intern_namespace("evil");
        fix.intern_namespace("acme");
        let a = caller("acme", ACME_SPACE);
        let b = caller("evil", EVIL_SPACE);

        // A's own statemented entity.
        let alpha = create_entity(&fix, a.clone(), [1; 16], "Alpha Public").await;
        create_statement(&fix, a.clone(), [2; 16], alpha, "lead").await;

        // B's secret statement — the row a forged floor would leak.
        let beta = create_entity(&fix, b.clone(), [3; 16], "BetaSecret").await;
        create_statement(
            &fix,
            b.clone(),
            [4; 16],
            beta,
            "BetaSecret Confidential Salary",
        )
        .await;

        // A legitimate first-page fetch never carries B's secret value.
        let clean = fetch(&fix, a.clone()).await;
        assert!(
            !clean.nodes.iter().any(|n| n.label.contains("Confidential")),
            "B's statement value leaked into A's clean page: {:?}",
            clean.nodes
        );

        // The forged out-of-tenant cursor is rejected outright — it never
        // becomes a scan floor, so B's rows cannot surface.
        let mut req = fetch_request();
        req.cursor = forged_zero_scope_cursor();
        let outcome = dispatch(RequestBody::GraphFetch(req), a, &fix.ctx).await;
        assert!(
            outcome.is_err(),
            "TENANT BREACH: forged ns=0/space=0 cursor must be rejected, got {outcome:?}"
        );
    })
}
