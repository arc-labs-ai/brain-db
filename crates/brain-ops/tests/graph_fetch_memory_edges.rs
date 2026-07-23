//! `GRAPH_FETCH` — the memory-edge layer (`include_memory_edges`).
//!
//! The export's spine is the subject-anchored statement index, so memories
//! sit two hops out (statement → entity → mentioning memory) and the stored
//! memory↔memory edges hang off *those*. These tests pin the four things
//! that makes non-obvious:
//!
//! 1. With the flag off, nothing changes — no edge kind above the four
//!    typed-graph projections appears, and no memory reaches the page that
//!    wasn't already reachable through `Mentions`.
//! 2. With it on, each builtin kind arrives on its own wire byte, so
//!    `SimilarTo` is distinguishable from `FollowedBy` per edge.
//! 3. An edge whose far endpoint isn't on the page still ships, with that
//!    endpoint emitted as a node — completeness over disjointness.
//! 4. Paginating doesn't lose edges, and toggling the layer mid-scroll is
//!    rejected rather than silently resumed against a differently-shaped
//!    export.
//!
//! The graph is built directly against the metadata tables the handler
//! reads (edges + texts) rather than through ENCODE: the extractor pipeline
//! decides *which* memory edges exist, and this is a test of the read path,
//! not of `auto_edge`'s judgement.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::sync::Arc;

use brain_core::{EdgeKind, EdgeKindRef, MemoryId, NodeRef};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{IndexParams, SharedHnsw};
use brain_metadata::tables::edge::{
    link, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE, EDGES_TABLE,
};
use brain_metadata::tables::text::TEXTS_TABLE;
use brain_metadata::MetadataDb;
use brain_ops::test_support::{run_in_glommio, single_body};
use brain_ops::{dispatch, OpsContext, RealWriterHandle, RequestCaller};
use brain_planner::{ExecutorContext, SharedMetadataDb, WriterHandle};
use brain_protocol::envelope::request::RequestBody;
use brain_protocol::envelope::response::ResponseBody;
use brain_protocol::{
    EntityCreateRequest, EntityCreateResponse, EvidenceRefWire, GraphEdge, GraphFetchRequest,
    GraphFetchResponseFrame, StatementCreateRequest, StatementCreateResponse, StatementKindWire,
    StatementObjectWire, StatementValueWire,
};

// ---------------------------------------------------------------------------
// Fixture.
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
    // The context keeps paths into the tempdir; hold it for the process.
    std::mem::forget(tempdir);
    Fixture { ctx, metadata }
}

impl Fixture {
    fn intern_namespace(&self, name: &str) {
        let wtxn = self.metadata.write_txn().expect("write txn");
        brain_metadata::namespace::namespace_intern_or_get(&wtxn, name, 0).expect("intern");
        wtxn.commit().expect("commit");
    }

    /// Write one edge row straight into the unified edge tables. Symmetric
    /// builtin kinds auto-mirror inside `link`, exactly as the workers see.
    fn link_edge(&self, from: NodeRef, kind: EdgeKindRef, to: NodeRef) {
        let wtxn = self.metadata.write_txn().expect("write txn");
        {
            let mut e = wtxn.open_table(EDGES_TABLE).expect("edges");
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).expect("edges_reverse");
            let data = EdgeData::new(
                0.9,
                brain_metadata::tables::edge::origin::AUTO_DERIVED,
                brain_metadata::tables::edge::derived_by::SIMILARITY_WORKER,
                1_700_000_000_000_000_000,
            );
            link(&mut e, &mut r, from, kind, to, zero_disambiguator(), &data).expect("link");
        }
        wtxn.commit().expect("commit");
    }

    fn put_text(&self, id: MemoryId, text: &str) {
        let wtxn = self.metadata.write_txn().expect("write txn");
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).expect("texts");
            t.insert(&id.to_be_bytes(), text.as_bytes())
                .expect("insert");
        }
        wtxn.commit().expect("commit");
    }
}

const SPACE: [u8; 16] = [0xA1; 16];

fn caller() -> RequestCaller {
    RequestCaller::from_scope(
        brain_core::SpaceId(uuid::Uuid::from_bytes(SPACE)),
        [0u8; 16],
        [0u8; 16],
        "acme".to_string(),
        brain_metadata::api_keys::bits::FULL,
    )
}

fn mem(slot: u64) -> MemoryId {
    MemoryId::pack(0, slot, 1)
}

// The five memories. M1..M3 mention Alpha, M4 mentions Beta, M5 mentions
// nothing — it is reachable only as the far endpoint of a memory edge.
const M1: u64 = 1;
const M2: u64 = 2;
const M3: u64 = 3;
const M4: u64 = 4;
const M5: u64 = 5;

// Wire kind bytes under test (mirror `GraphEdgeKindWire`).
const KIND_MENTIONS: u8 = 3;
const KIND_CAUSED: u8 = 4;
const KIND_FOLLOWED_BY: u8 = 5;
const KIND_SIMILAR_TO: u8 = 7;

// ---------------------------------------------------------------------------
// Drivers.
// ---------------------------------------------------------------------------

async fn create_entity(fix: &Fixture, request_id: [u8; 16], canonical: &str) -> [u8; 16] {
    let req = EntityCreateRequest {
        entity_type_id: 1,
        canonical_name: canonical.to_string(),
        aliases: Vec::new(),
        attributes_blob: Vec::new(),
        request_id,
        act_as: None,
    };
    match single_body(
        dispatch(RequestBody::EntityCreate(req), caller(), &fix.ctx)
            .await
            .expect("entity_create"),
    ) {
        ResponseBody::EntityCreate(EntityCreateResponse { entity_id }) => entity_id,
        other => panic!("expected EntityCreate, got {other:?}"),
    }
}

async fn create_statement(fix: &Fixture, request_id: [u8; 16], subject: [u8; 16], value: &str) {
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
        request_id,
        act_as: None,
    };
    match single_body(
        dispatch(RequestBody::StatementCreate(req), caller(), &fix.ctx)
            .await
            .expect("statement_create"),
    ) {
        ResponseBody::StatementCreate(StatementCreateResponse { .. }) => {}
        other => panic!("expected StatementCreate, got {other:?}"),
    }
}

fn fetch_request(limit: u32, cursor: Vec<u8>, memory_edges: bool) -> GraphFetchRequest {
    GraphFetchRequest {
        limit,
        cursor,
        include_statements: false,
        include_memories: true,
        include_memory_edges: memory_edges,
        include_tombstoned: false,
        act_as: None,
    }
}

async fn fetch(fix: &Fixture, req: GraphFetchRequest) -> GraphFetchResponseFrame {
    match single_body(
        dispatch(RequestBody::GraphFetch(req), caller(), &fix.ctx)
            .await
            .expect("graph_fetch"),
    ) {
        ResponseBody::GraphFetch(f) => f,
        other => panic!("expected GraphFetch, got {other:?}"),
    }
}

/// The full graph under test. Two statemented entities give the spine two
/// rows, so `limit = 1` genuinely paginates.
async fn seed_graph(fix: &Fixture) {
    fix.intern_namespace("acme");
    let alpha = create_entity(fix, [1; 16], "Alpha").await;
    let beta = create_entity(fix, [2; 16], "Beta").await;
    create_statement(fix, [3; 16], alpha, "lead").await;
    create_statement(fix, [4; 16], beta, "ic").await;

    for (slot, text) in [
        (M1, "the deploy pipeline runs on merge"),
        (M2, "the deploy pipeline is triggered by a merge"),
        (M3, "the deploy pipeline was rewritten last quarter"),
        (M4, "beta owns the release checklist"),
        (M5, "an unmentioned note about pipelines"),
    ] {
        fix.put_text(mem(slot), text);
    }

    for (slot, entity) in [(M1, alpha), (M2, alpha), (M3, alpha), (M4, beta)] {
        fix.link_edge(
            NodeRef::Memory(mem(slot)),
            EdgeKindRef::Mentions,
            NodeRef::Entity(brain_core::EntityId::from_bytes(entity)),
        );
    }

    // Symmetric: one logical pair, stored both ways.
    fix.link_edge(
        NodeRef::Memory(mem(M1)),
        EdgeKindRef::Builtin(EdgeKind::SimilarTo),
        NodeRef::Memory(mem(M2)),
    );
    // Symmetric, far endpoint off the page entirely.
    fix.link_edge(
        NodeRef::Memory(mem(M1)),
        EdgeKindRef::Builtin(EdgeKind::SimilarTo),
        NodeRef::Memory(mem(M5)),
    );
    // Asymmetric, both endpoints on Alpha's page.
    fix.link_edge(
        NodeRef::Memory(mem(M2)),
        EdgeKindRef::Builtin(EdgeKind::FollowedBy),
        NodeRef::Memory(mem(M3)),
    );
    // Asymmetric, crossing from Alpha's memories to Beta's.
    fix.link_edge(
        NodeRef::Memory(mem(M3)),
        EdgeKindRef::Builtin(EdgeKind::Caused),
        NodeRef::Memory(mem(M4)),
    );
}

fn edge_set(frame: &GraphFetchResponseFrame) -> BTreeSet<([u8; 16], [u8; 16], u8)> {
    frame
        .edges
        .iter()
        .map(|e| (e.from_id, e.to_id, e.kind))
        .collect()
}

fn node_set(frame: &GraphFetchResponseFrame) -> BTreeSet<[u8; 16]> {
    frame.nodes.iter().map(|n| n.id).collect()
}

fn find(frame: &GraphFetchResponseFrame, kind: u8) -> Vec<&GraphEdge> {
    frame.edges.iter().filter(|e| e.kind == kind).collect()
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

/// With the flag off the export is exactly what it was: only the four
/// typed-graph edge bytes, and no memory that `Mentions` didn't already
/// reach.
#[test]
fn flag_off_emits_no_memory_edges() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let page = fetch(&fix, fetch_request(500, Vec::new(), false)).await;

        assert!(
            page.edges.iter().all(|e| e.kind <= KIND_MENTIONS),
            "flag off must not emit builtin edge kinds; got {:?}",
            page.edges.iter().map(|e| e.kind).collect::<Vec<_>>()
        );
        assert_eq!(find(&page, KIND_MENTIONS).len(), 4, "the four mentions");
        assert!(
            !node_set(&page).contains(&mem(M5).to_be_bytes()),
            "M5 mentions nothing, so it must not surface without the flag"
        );
    })
}

/// Turning the flag on is purely additive: every node and edge from the
/// flag-off page is still present, byte-for-byte identical.
#[test]
fn flag_on_is_additive_over_flag_off() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let off = fetch(&fix, fetch_request(500, Vec::new(), false)).await;
        let on = fetch(&fix, fetch_request(500, Vec::new(), true)).await;

        assert!(edge_set(&off).is_subset(&edge_set(&on)));
        assert!(node_set(&off).is_subset(&node_set(&on)));
        // Restricting the flag-on page to the typed-graph kinds reproduces
        // the flag-off page exactly.
        let on_projections: BTreeSet<_> = edge_set(&on)
            .into_iter()
            .filter(|(_, _, k)| *k <= KIND_MENTIONS)
            .collect();
        assert_eq!(on_projections, edge_set(&off));
        for edge in &off.edges {
            assert!(on.edges.contains(edge), "{edge:?} lost when flag turned on");
        }
    })
}

/// Each builtin kind lands on its own wire byte with its own label, and
/// asymmetric edges keep the stored direction.
#[test]
fn memory_edges_carry_distinguishable_kinds_and_endpoints() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let page = fetch(&fix, fetch_request(500, Vec::new(), true)).await;

        let similar = find(&page, KIND_SIMILAR_TO);
        assert_eq!(similar.len(), 2, "M1↔M2 and M1↔M5, once each: {similar:?}");
        for e in &similar {
            assert_eq!(e.label, "similar_to");
        }
        // Symmetric kinds are collapsed onto one id-ordered representative,
        // so the mirrored storage row does not become a second edge.
        let similar_pairs: BTreeSet<_> = similar
            .iter()
            .map(|e| {
                let mut p = [e.from_id, e.to_id];
                p.sort_unstable();
                p
            })
            .collect();
        assert_eq!(similar_pairs.len(), 2);

        let followed = find(&page, KIND_FOLLOWED_BY);
        assert_eq!(followed.len(), 1, "one FollowedBy: {followed:?}");
        assert_eq!(followed[0].from_id, mem(M2).to_be_bytes());
        assert_eq!(followed[0].to_id, mem(M3).to_be_bytes());
        assert_eq!(followed[0].label, "followed_by");

        let caused = find(&page, KIND_CAUSED);
        assert_eq!(caused.len(), 1, "one Caused: {caused:?}");
        assert_eq!(caused[0].from_id, mem(M3).to_be_bytes());
        assert_eq!(caused[0].to_id, mem(M4).to_be_bytes());
        assert_eq!(caused[0].label, "caused");

        // The kind byte alone separates them — no companion field needed.
        assert_ne!(KIND_SIMILAR_TO, KIND_FOLLOWED_BY);
    })
}

/// An edge to a memory that mentions no page entity still ships, and its
/// far endpoint is emitted as a labelled node so nothing dangles.
#[test]
fn far_endpoint_memory_is_emitted_as_a_node() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let page = fetch(&fix, fetch_request(500, Vec::new(), true)).await;

        let m5 = mem(M5).to_be_bytes();
        let node = page
            .nodes
            .iter()
            .find(|n| n.id == m5)
            .expect("M5 must surface as the far endpoint of SimilarTo(M1, M5)");
        assert_eq!(node.kind, 2, "memory node kind");
        assert_eq!(node.label, "an unmentioned note about pipelines");

        // Every edge endpoint resolves to a node on the same page.
        let nodes = node_set(&page);
        for e in &page.edges {
            assert!(nodes.contains(&e.from_id), "dangling from_id in {e:?}");
            assert!(nodes.contains(&e.to_id), "dangling to_id in {e:?}");
        }
    })
}

/// Paging one statement at a time yields the same union of nodes and edges
/// as a single page. Repeats across pages are allowed (completeness, not
/// disjointness); losses are not.
#[test]
fn pagination_loses_no_memory_edges() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let whole = fetch(&fix, fetch_request(500, Vec::new(), true)).await;

        let mut edges = BTreeSet::new();
        let mut nodes = BTreeSet::new();
        let mut cursor = Vec::new();
        let mut pages = 0;
        loop {
            let page = fetch(&fix, fetch_request(1, cursor.clone(), true)).await;
            edges.extend(edge_set(&page));
            nodes.extend(node_set(&page));
            pages += 1;
            assert!(pages < 16, "cursor failed to terminate");
            if page.next_cursor.is_empty() {
                break;
            }
            cursor = page.next_cursor;
        }

        assert!(pages > 1, "the two-statement spine must actually paginate");
        assert_eq!(edges, edge_set(&whole), "paged edge union differs");
        assert_eq!(nodes, node_set(&whole), "paged node union differs");
    })
}

/// The flag is bound into the cursor, so flipping it mid-scroll is a stale
/// cursor rather than a page from a differently-shaped export.
#[test]
fn cursor_rejects_memory_edge_toggle_mid_scroll() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let first = fetch(&fix, fetch_request(1, Vec::new(), true)).await;
        assert!(!first.next_cursor.is_empty(), "expected more pages");

        let resumed = dispatch(
            RequestBody::GraphFetch(fetch_request(1, first.next_cursor.clone(), false)),
            caller(),
            &fix.ctx,
        )
        .await;
        assert!(resumed.is_err(), "toggling the layer must invalidate");

        // Same cursor, same flags, still fine.
        let ok = fetch(&fix, fetch_request(1, first.next_cursor, true)).await;
        assert!(!ok.nodes.is_empty());
    })
}

/// Memory edges hang off memory nodes: asking for them without the layer
/// that emits those nodes is rejected, not silently answered with edges the
/// client cannot place.
#[test]
fn memory_edges_without_memories_is_rejected() {
    run_in_glommio(|| async {
        let fix = build_fixture();
        seed_graph(&fix).await;

        let mut req = fetch_request(500, Vec::new(), true);
        req.include_memories = false;
        let out = dispatch(RequestBody::GraphFetch(req), caller(), &fix.ctx).await;
        assert!(
            out.is_err(),
            "include_memory_edges requires include_memories"
        );
    })
}
