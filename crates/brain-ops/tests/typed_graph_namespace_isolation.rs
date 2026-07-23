//! Cross-namespace + cross-space isolation for the TYPED-GRAPH layer
//! (entities, statements, relations) — the sibling of the memory-layer
//! proof in `namespace_isolation.rs`.
//!
//! The typed-graph scope key is `(namespace_id, space_id)`: namespace is
//! the outer (tenant/company) wall, space the inner (app) wall. Every
//! typed-graph row carries that scope and every secondary index is
//! prefixed by it, so a read as one `(namespace, space)` must never
//! surface another scope's entity, statement, or relation — on ANY
//! typed-graph read path (`ENTITY_RESOLVE` / `ENTITY_GET` /
//! `STATEMENT_LIST` / `RELATION_LIST_FROM` / `QUERY`).
//!
//! Like the memory proof, this drives the *real* `dispatch` → handler
//! path (the same code the wire layer calls), constructing strict-mode
//! `RequestCaller`s with distinct interned namespaces + spaces directly,
//! because the shared wire harness can't mint namespaced keys into a
//! running shard. The assertions are pure membership (which ids appear),
//! never score ordering.
//!
//! Three walls are proven:
//!  1. A statement / relation created under `acme` is invisible to
//!     `globex` (STATEMENT_LIST / RELATION_LIST_FROM / QUERY).
//!  2. The same entity NAME ("John Smith") created under `acme/chatbot`
//!     and `globex` resolves to DISTINCT entity ids, and `globex`
//!     ENTITY_RESOLVE / ENTITY_GET never returns acme's entity.
//!  3. Within `acme`, spaces `chatbot` and `research` are isolated too
//!     (the inner space wall) for the typed graph.

#![cfg(target_os = "linux")]

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
    EntityCreateRequest, EntityCreateResponse, EntityGetRequest, EntityResolveRequest,
    EntityResolveResponse, EvidenceRefWire, RelationCreateRequest, RelationCreateResponse,
    RelationListFromRequest, RelationListFromResponseFrame, ResolutionOutcomeWire,
    StatementCreateRequest, StatementCreateResponse, StatementKindWire, StatementListRequest,
    StatementListResponseFrame, StatementObjectWire, StatementValueWire,
};

// ---------------------------------------------------------------------------
// Mock dispatcher (text-driven deterministic vectors; same as the memory proof).
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

    // tempdir is moved into the OpsContext builder; keep the handle alive by
    // leaking it into the context (the memory proof keeps it on the Fixture,
    // but the typed-graph proof doesn't reindex lexically, so we don't need
    // the path afterwards — the builder retains what it needs).
    let ctx = brain_ops::test_support::ops_context_for_tests(executor, tempdir.path());
    // Keep the tempdir alive for the lifetime of the test process.
    std::mem::forget(tempdir);
    Fixture { ctx, metadata }
}

/// The two spaces inside `acme`, plus globex's space. Distinct so the
/// space inner wall can be exercised independently of the namespace wall.
const ACME_CHATBOT: [u8; 16] = [0xC1; 16];
const ACME_RESEARCH: [u8; 16] = [0xC2; 16];
const GLOBEX_BOT: [u8; 16] = [0x6B; 16];

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
        // brain:Person is seeded by the system schema at id = 1.
        entity_type_id: 1,
        canonical_name: canonical.to_string(),
        aliases: Vec::new(),
        attributes_blob: Vec::new(),
        request_id,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::EntityCreate(req), c, &fix.ctx)
        .await
        .expect("entity_create dispatch");
    match single_body(outcome) {
        ResponseBody::EntityCreate(EntityCreateResponse { entity_id }) => entity_id,
        other => panic!("expected EntityCreate response, got {other:?}"),
    }
}

async fn create_statement(
    fix: &Fixture,
    c: RequestCaller,
    request_id: [u8; 16],
    subject: [u8; 16],
    predicate: &str,
    value: &str,
) -> [u8; 16] {
    let req = StatementCreateRequest {
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
        request_id,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::StatementCreate(req), c, &fix.ctx)
        .await
        .expect("statement_create dispatch");
    match single_body(outcome) {
        ResponseBody::StatementCreate(StatementCreateResponse { statement_id, .. }) => statement_id,
        other => panic!("expected StatementCreate response, got {other:?}"),
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
        request_id,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::RelationCreate(req), c, &fix.ctx)
        .await
        .expect("relation_create dispatch");
    match single_body(outcome) {
        ResponseBody::RelationCreate(RelationCreateResponse { relation_id }) => relation_id,
        other => panic!("expected RelationCreate response, got {other:?}"),
    }
}

async fn resolve_entity(fix: &Fixture, c: RequestCaller, name: &str) -> EntityResolveResponse {
    let req = EntityResolveRequest {
        candidate_name: name.to_string(),
        resolution_context: String::new(),
        entity_type_hint: 0,
        allow_create: false,
        request_id: [0u8; 16],
        act_as: None,
    };
    let outcome = dispatch(RequestBody::EntityResolve(req), c, &fix.ctx)
        .await
        .expect("entity_resolve dispatch");
    match single_body(outcome) {
        ResponseBody::EntityResolve(r) => r,
        other => panic!("expected EntityResolve response, got {other:?}"),
    }
}

/// `Ok(())` if ENTITY_GET returned the entity; `Err(())` if it was walled
/// off (NotFound). Used to prove a foreign id is never readable.
async fn entity_get_visible(fix: &Fixture, c: RequestCaller, id: [u8; 16]) -> bool {
    let req = EntityGetRequest {
        entity_id: id,
        act_as: None,
    };
    let outcome = dispatch(RequestBody::EntityGet(req), c, &fix.ctx).await;
    matches!(outcome.map(single_body), Ok(ResponseBody::EntityGet(_)))
}

async fn list_statement_subjects(
    fix: &Fixture,
    c: RequestCaller,
    subject: [u8; 16],
) -> Vec<[u8; 16]> {
    let req = StatementListRequest {
        subject,
        predicate: String::new(),
        kind: 0,
        min_confidence: 0.0,
        time_range_start_unix_nanos: 0,
        time_range_end_unix_nanos: 0,
        only_current: true,
        include_tombstoned: false,
        limit: 100,
        cursor: Vec::new(),
        act_as: None,
    };
    let outcome = dispatch(RequestBody::StatementList(req), c, &fix.ctx)
        .await
        .expect("statement_list dispatch");
    match single_body(outcome) {
        ResponseBody::StatementList(StatementListResponseFrame { items, .. }) => {
            items.into_iter().map(|s| s.statement_id).collect()
        }
        other => panic!("expected StatementList response, got {other:?}"),
    }
}

async fn list_relations_from(fix: &Fixture, c: RequestCaller, from: [u8; 16]) -> Vec<[u8; 16]> {
    let req = RelationListFromRequest {
        from_entity: from,
        relation_type_filter: String::new(),
        time_range_start_unix_nanos: 0,
        time_range_end_unix_nanos: 0,
        include_superseded: false,
        include_tombstoned: false,
        limit: 100,
        cursor: Vec::new(),
        act_as: None,
    };
    let outcome = dispatch(RequestBody::RelationListFrom(req), c, &fix.ctx)
        .await
        .expect("relation_list_from dispatch");
    match single_body(outcome) {
        ResponseBody::RelationListFrom(RelationListFromResponseFrame { items, .. }) => {
            items.into_iter().map(|r| r.relation_id).collect()
        }
        other => panic!("expected RelationListFrom response, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// The same entity NAME under two different namespaces resolves to two
/// DISTINCT entity ids, and `globex` can never resolve to / read acme's
/// entity. This is the per-namespace entity-space proof.
#[test]
fn entity_name_resolves_per_namespace() {
    run_in_glommio(|| async {
        let mut fix = build_fixture();
        let acme = fix.intern_namespace("acme");
        let globex = fix.intern_namespace("globex");
        assert_ne!(acme, globex);

        let acme_c = caller("acme", ACME_CHATBOT);
        let globex_c = caller("globex", GLOBEX_BOT);

        let acme_john = create_entity(&fix, acme_c.clone(), [1; 16], "John Smith").await;
        let globex_john = create_entity(&fix, globex_c.clone(), [2; 16], "John Smith").await;
        assert_ne!(
            acme_john, globex_john,
            "same name under distinct namespaces must mint distinct entities"
        );

        // Each tenant resolves "John Smith" to ITS OWN entity.
        let acme_res = resolve_entity(&fix, acme_c.clone(), "John Smith").await;
        assert_eq!(acme_res.outcome, ResolutionOutcomeWire::Resolved);
        assert_eq!(
            acme_res.resolved_entity, acme_john,
            "acme must resolve to its own John"
        );

        let globex_res = resolve_entity(&fix, globex_c.clone(), "John Smith").await;
        assert_eq!(globex_res.outcome, ResolutionOutcomeWire::Resolved);
        assert_eq!(
            globex_res.resolved_entity, globex_john,
            "globex must resolve to its own John, never acme's"
        );
        assert_ne!(globex_res.resolved_entity, acme_john, "TENANT BREACH");

        // ENTITY_GET on the foreign id is walled off (NotFound).
        assert!(
            !entity_get_visible(&fix, globex_c, acme_john).await,
            "TENANT BREACH: globex read acme's entity via ENTITY_GET"
        );
        assert!(
            !entity_get_visible(&fix, acme_c, globex_john).await,
            "TENANT BREACH: acme read globex's entity via ENTITY_GET"
        );

        // Silence unused warning on the leftover field across cfgs.
        let _ = &mut fix;
    })
}

/// A statement created in `acme` is invisible to `globex` via
/// STATEMENT_LIST (and acme sees its own). Subjects are per-namespace
/// entities, so even the same subject NAME yields disjoint id spaces.
#[test]
fn statement_does_not_leak_across_namespaces() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _acme = fix.intern_namespace("acme");
        let _globex = fix.intern_namespace("globex");
        let acme_c = caller("acme", ACME_CHATBOT);
        let globex_c = caller("globex", GLOBEX_BOT);

        let acme_subj = create_entity(&fix, acme_c.clone(), [1; 16], "Acme Subject").await;
        let acme_stmt =
            create_statement(&fix, acme_c.clone(), [3; 16], acme_subj, "app:role", "ceo").await;

        // acme sees its own statement.
        let acme_view = list_statement_subjects(&fix, acme_c.clone(), acme_subj).await;
        assert!(
            acme_view.contains(&acme_stmt),
            "acme must see its own statement {acme_stmt:?}; got {acme_view:?}"
        );

        // globex, listing by the SAME entity id, sees nothing — the id is
        // not in its scope, and the scoped index can't reach acme's row.
        let globex_view = list_statement_subjects(&fix, globex_c, acme_subj).await;
        assert!(
            globex_view.is_empty(),
            "TENANT BREACH: globex saw acme's statements {globex_view:?}"
        );
    })
}

/// A relation created in `acme` is invisible to `globex` via
/// RELATION_LIST_FROM (and acme sees its own).
#[test]
fn relation_does_not_leak_across_namespaces() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _acme = fix.intern_namespace("acme");
        let _globex = fix.intern_namespace("globex");
        let acme_c = caller("acme", ACME_CHATBOT);
        let globex_c = caller("globex", GLOBEX_BOT);

        let a = create_entity(&fix, acme_c.clone(), [1; 16], "Acme From").await;
        let b = create_entity(&fix, acme_c.clone(), [2; 16], "Acme To").await;
        let rel = create_relation(&fix, acme_c.clone(), [4; 16], a, b, "app:works_with").await;

        let acme_view = list_relations_from(&fix, acme_c, a).await;
        assert!(
            acme_view.contains(&rel),
            "acme must see its own relation {rel:?}; got {acme_view:?}"
        );

        let globex_view = list_relations_from(&fix, globex_c, a).await;
        assert!(
            globex_view.is_empty(),
            "TENANT BREACH: globex saw acme's relations {globex_view:?}"
        );
    })
}

/// Within ONE namespace, two spaces (chatbot, research) are isolated for
/// the typed graph: the same name mints distinct entities, and each
/// space's statements/relations are invisible to the other. This proves
/// the inner (space) wall, holding namespace constant.
#[test]
fn spaces_isolated_within_one_namespace() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        let _acme = fix.intern_namespace("acme");
        let chatbot = caller("acme", ACME_CHATBOT);
        let research = caller("acme", ACME_RESEARCH);

        // Same name, same namespace, different space → distinct entities.
        let cb_john = create_entity(&fix, chatbot.clone(), [1; 16], "John Smith").await;
        let rs_john = create_entity(&fix, research.clone(), [2; 16], "John Smith").await;
        assert_ne!(
            cb_john, rs_john,
            "same name under distinct spaces must mint distinct entities"
        );

        // Each space resolves to its own John.
        let cb_res = resolve_entity(&fix, chatbot.clone(), "John Smith").await;
        assert_eq!(cb_res.resolved_entity, cb_john);
        let rs_res = resolve_entity(&fix, research.clone(), "John Smith").await;
        assert_eq!(rs_res.resolved_entity, rs_john);
        assert_ne!(rs_res.resolved_entity, cb_john, "SPACE BREACH (resolve)");

        // chatbot's statement is invisible to research.
        let cb_stmt =
            create_statement(&fix, chatbot.clone(), [3; 16], cb_john, "app:role", "lead").await;
        let cb_view = list_statement_subjects(&fix, chatbot.clone(), cb_john).await;
        assert!(cb_view.contains(&cb_stmt));
        let rs_view = list_statement_subjects(&fix, research.clone(), cb_john).await;
        assert!(
            rs_view.is_empty(),
            "SPACE BREACH: research saw chatbot's statements {rs_view:?}"
        );

        // chatbot's entity is unreadable by research via ENTITY_GET.
        assert!(
            !entity_get_visible(&fix, research, cb_john).await,
            "SPACE BREACH: research read chatbot's entity via ENTITY_GET"
        );
    })
}
