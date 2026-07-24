//! Unit tests for `BrainSemanticRetriever`.

use std::sync::Arc;

use brain_core::{SpaceId, SessionId, MemoryId, MemoryKind};
use brain_embed::{Dispatcher, EmbedError, VECTOR_DIM};
use brain_index::{
    IndexParams, RankedItemId, SemanticError, SemanticFilters, SemanticFiltersConfigSlot,
    SemanticQuery, SemanticRetriever, SemanticRetrieverConfig, SemanticScope, SharedHnsw,
    SEMANTIC_EF_SEARCH_MAX,
};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use tempfile::TempDir;

use super::BrainSemanticRetriever;

// ---------------------------------------------------------------------------
// Fixtures.
// ---------------------------------------------------------------------------

/// Embedder that returns the provided vector when asked for any
/// text; ignores the input. Lets tests pretend the query text
/// "matches" a known memory.
struct FixedDispatcher {
    vector: [f32; VECTOR_DIM],
    fingerprint: [u8; 16],
}

impl Dispatcher for FixedDispatcher {
    fn embed(&self, _text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
        Ok(self.vector)
    }
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
        Ok(texts.iter().map(|_| self.vector).collect())
    }
    fn fingerprint(&self) -> [u8; 16] {
        self.fingerprint
    }
}

/// A near-orthonormal vector — first slot is one-hot.
fn one_hot(slot: usize) -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    v[slot % VECTOR_DIM] = 1.0;
    v
}

fn fresh_metadata() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().expect("tempdir");
    let db = MetadataDb::open(dir.path().join("metadata.redb")).expect("open metadata");
    (dir, db)
}

fn write_memory_row(
    metadata: &mut MetadataDb,
    id: MemoryId,
    space: SpaceId,
    kind: MemoryKind,
    created_at_unix_ms: u64,
) {
    write_memory_row_ns(
        metadata,
        id,
        brain_core::NamespaceId::SYSTEM,
        space,
        kind,
        created_at_unix_ms,
    );
}

/// Like `write_memory_row` but lets the test choose the namespace, so the
/// tenant-wall cases can seed a row that must be excluded for a caller
/// scoped to a different namespace.
fn write_memory_row_ns(
    metadata: &mut MetadataDb,
    id: MemoryId,
    namespace: brain_core::NamespaceId,
    space: SpaceId,
    kind: MemoryKind,
    created_at_unix_ms: u64,
) {
    let mem = MemoryMetadata::new_active(
        id,
        namespace,
        space,
        SessionId::from(0),
        id.slot(),
        id.version(),
        kind,
        [0u8; 16],
        0.5,
        0,
        created_at_unix_ms.saturating_mul(1_000_000),
    );
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut table = wtxn.open_table(MEMORIES_TABLE).expect("open");
        table.insert(&id.raw().to_be_bytes(), &mem).expect("insert");
    }
    wtxn.commit().expect("commit");
}

fn build_retriever(metadata: MetadataDb) -> BrainSemanticRetriever {
    let (reader, _writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw::new");
    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata))
}

// ---------------------------------------------------------------------------
// Scope dispatch / validation.
// ---------------------------------------------------------------------------

#[test]
fn statement_scope_without_handle_returns_empty() {
    let (_dir, metadata) = fresh_metadata();
    let retriever = build_retriever(metadata);

    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Statement,
            &SemanticRetrieverConfig::default(),
            None,
        )
        .expect("retrieve");
    assert!(result.is_empty());
}

#[test]
fn ef_search_above_max_errors() {
    let (_dir, metadata) = fresh_metadata();
    let retriever = build_retriever(metadata);

    let cfg = SemanticRetrieverConfig {
        ef_search: SEMANTIC_EF_SEARCH_MAX + 1,
        ..Default::default()
    };
    let err = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            None,
        )
        .expect_err("rejects");
    assert!(matches!(err, SemanticError::QueryParseFailed(_)));
}

#[test]
fn wrong_scope_filter_errors() {
    let (_dir, metadata) = fresh_metadata();
    let retriever = build_retriever(metadata);

    let cfg = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(SemanticFilters {
            namespace_id: brain_core::NamespaceId::SYSTEM.raw(),
            predicate_id: Some(brain_core::PredicateId::from(7)),
            ..Default::default()
        }),
        ..Default::default()
    };
    let err = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            None,
        )
        .expect_err("rejects");
    assert!(matches!(err, SemanticError::QueryParseFailed(_)));
}

#[test]
fn empty_memory_corpus_returns_no_hits() {
    let (_dir, metadata) = fresh_metadata();
    let retriever = build_retriever(metadata);

    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &SemanticRetrieverConfig::default(),
            None,
        )
        .expect("retrieve");
    assert!(result.is_empty());
}

// ---------------------------------------------------------------------------
// Memory scope end-to-end (insert into SharedHnsw + filter through redb).
// ---------------------------------------------------------------------------

#[test]
fn memory_scope_returns_ranked_hits() {
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);

    writer.insert(id1, &one_hot(0)).expect("insert id1");
    writer.insert(id2, &one_hot(10)).expect("insert id2");

    write_memory_row(&mut metadata, id1, space, MemoryKind::Episodic, 1_000);
    write_memory_row(&mut metadata, id2, space, MemoryKind::Episodic, 2_000);

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &SemanticRetrieverConfig::default(),
            None,
        )
        .expect("retrieve");

    assert!(!result.is_empty(), "must return at least one hit");
    // The first-slot one-hot must be the top hit.
    match result[0].id {
        RankedItemId::Memory(id) => assert_eq!(id, id1),
        other => panic!("expected MemoryId, got {other:?}"),
    }
    assert_eq!(result[0].rank, 1);
}

#[test]
fn space_id_filter_narrows() {
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space_a = SpaceId::new();
    let space_b = SpaceId::new();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);

    writer.insert(id1, &one_hot(0)).expect("ins1");
    writer.insert(id2, &one_hot(1)).expect("ins2");

    write_memory_row(&mut metadata, id1, space_a, MemoryKind::Episodic, 0);
    write_memory_row(&mut metadata, id2, space_b, MemoryKind::Episodic, 0);

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    let cfg = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(SemanticFilters {
            namespace_id: brain_core::NamespaceId::SYSTEM.raw(),
            space_ids: vec![space_a],
            ..Default::default()
        }),
        top_k: 10,
        ..Default::default()
    };

    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            None,
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1, "space filter must select exactly id1");
    if let RankedItemId::Memory(id) = result[0].id {
        assert_eq!(id, id1);
    } else {
        panic!("expected Memory id");
    }
}

#[test]
fn namespace_filter_excludes_foreign_namespace() {
    // The tenant wall: two memories sit in the HNSW with identical (top-hit)
    // vectors, one in the caller's namespace and one in a foreign namespace.
    // A recall scoped to the caller's namespace must return ONLY the
    // same-namespace memory — the foreign-namespace row is never visible,
    // regardless of how strong its vector match is.
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();
    let own_ns = brain_core::NamespaceId::from(7u32);
    let foreign_ns = brain_core::NamespaceId::from(9u32);
    let mine = MemoryId::pack(0, 1, 0);
    let theirs = MemoryId::pack(0, 2, 0);

    // Both index at the exact query vector so vector match cannot explain
    // the exclusion — only the namespace wall can.
    writer.insert(mine, &one_hot(0)).expect("insert mine");
    writer.insert(theirs, &one_hot(0)).expect("insert theirs");

    write_memory_row_ns(&mut metadata, mine, own_ns, space, MemoryKind::Episodic, 0);
    write_memory_row_ns(
        &mut metadata,
        theirs,
        foreign_ns,
        space,
        MemoryKind::Episodic,
        0,
    );

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    let cfg = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(SemanticFilters {
            namespace_id: own_ns.raw(),
            ..Default::default()
        }),
        top_k: 10,
        ..Default::default()
    };

    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            None,
        )
        .expect("retrieve");

    let ids: Vec<MemoryId> = result
        .iter()
        .filter_map(|r| match r.id {
            RankedItemId::Memory(id) => Some(id),
            _ => None,
        })
        .collect();
    assert!(
        ids.contains(&mine),
        "own-namespace memory must be returned, got {ids:?}"
    );
    assert!(
        !ids.contains(&theirs),
        "foreign-namespace memory must be excluded by the tenant wall, got {ids:?}"
    );
}

#[test]
fn created_at_range_filter_narrows() {
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);
    let id3 = MemoryId::pack(0, 3, 0);

    writer.insert(id1, &one_hot(0)).expect("ins1");
    writer.insert(id2, &one_hot(1)).expect("ins2");
    writer.insert(id3, &one_hot(2)).expect("ins3");

    write_memory_row(&mut metadata, id1, space, MemoryKind::Episodic, 100);
    write_memory_row(&mut metadata, id2, space, MemoryKind::Episodic, 500);
    write_memory_row(&mut metadata, id3, space, MemoryKind::Episodic, 900);

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    let cfg = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(SemanticFilters {
            namespace_id: brain_core::NamespaceId::SYSTEM.raw(),
            created_at_ms: Some(200..=800),
            ..Default::default()
        }),
        top_k: 10,
        ..Default::default()
    };

    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(1))),
            SemanticScope::Memory,
            &cfg,
            None,
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1, "only the middle doc should match");
}

#[test]
fn text_query_path_routes_through_embedder() {
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();
    let id = MemoryId::pack(0, 1, 0);

    writer.insert(id, &one_hot(0)).expect("ins");

    write_memory_row(&mut metadata, id, space, MemoryKind::Episodic, 0);

    // The embedder ignores its input and always returns one_hot(0).
    // Querying for an unrelated text still matches.
    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    let result = retriever
        .retrieve(
            &SemanticQuery::Text("totally unrelated text".into()),
            SemanticScope::Memory,
            &SemanticRetrieverConfig::default(),
            None,
        )
        .expect("retrieve");

    assert!(!result.is_empty(), "embedder path must reach HNSW");
}

#[test]
fn similarity_threshold_drops_low_scores() {
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();
    let id1 = MemoryId::pack(0, 1, 0);
    let id2 = MemoryId::pack(0, 2, 0);

    writer.insert(id1, &one_hot(0)).expect("ins1");
    writer.insert(id2, &one_hot(100)).expect("ins2");

    write_memory_row(&mut metadata, id1, space, MemoryKind::Episodic, 0);
    write_memory_row(&mut metadata, id2, space, MemoryKind::Episodic, 0);

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    // Threshold so high only the exact match survives.
    let cfg = SemanticRetrieverConfig {
        similarity_threshold: 0.95,
        top_k: 10,
        ..Default::default()
    };
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            None,
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1);
    assert!(result[0].score >= 0.95);
}

// ---------------------------------------------------------------------------
// Single-space brute-force lane.
// ---------------------------------------------------------------------------

use brain_index::SpaceVectorSource;
use brain_metadata::tables::memory::{space_timeline_key, MEMORIES_BY_SPACE_TIMELINE_TABLE};
use brain_metadata::tables::space::{space_key, SpaceMetadata, SPACES_TABLE};

/// In-memory `SpaceVectorSource` planted with `(slot, version) → vector`.
/// Stands in for the arena so the brute-force lane can be exercised
/// without a real mmap.
struct MockArena {
    vectors: std::collections::HashMap<(u64, u32), [f32; VECTOR_DIM]>,
}

impl SpaceVectorSource for MockArena {
    fn vector_at(&self, slot: u64, expected_version: u32) -> Option<[f32; VECTOR_DIM]> {
        self.vectors.get(&(slot, expected_version)).copied()
    }
}

/// Plant a memory: its `MEMORIES_TABLE` row + its
/// `MEMORIES_BY_SPACE_TIMELINE_TABLE` key (which the brute-force lane
/// range-scans). Returns the `MemoryId`.
fn plant_memory(
    metadata: &mut MetadataDb,
    slot: u64,
    version: u32,
    space: SpaceId,
    session_id: u64,
    created_ms: u64,
) -> MemoryId {
    let id = MemoryId::pack(0, slot, version);
    let ns = brain_core::NamespaceId::SYSTEM;
    let mem = MemoryMetadata::new_active(
        id,
        ns,
        space,
        SessionId::from(session_id),
        id.slot(),
        id.version(),
        MemoryKind::Semantic,
        [0u8; 16],
        0.5,
        0,
        created_ms.saturating_mul(1_000_000),
    );
    let space_bytes: [u8; 16] = space.into();
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open memories");
        t.insert(&id.raw().to_be_bytes(), &mem).expect("insert row");
        let mut tl = wtxn
            .open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE)
            .expect("open timeline");
        let key = space_timeline_key(
            ns.raw(),
            space_bytes,
            created_ms.saturating_mul(1_000_000),
            session_id,
            id.raw().to_be_bytes(),
        );
        tl.insert(&key[..], &()).expect("insert timeline");
    }
    wtxn.commit().expect("commit");
    id
}

/// Register a space in the SPACES table with an explicit `memory_count`
/// (the brute-force routing gate reads this).
fn register_space(metadata: &mut MetadataDb, space: SpaceId, memory_count: u64) {
    let space_bytes: [u8; 16] = space.into();
    let mut meta = SpaceMetadata::new(0, String::new(), None);
    meta.memory_count = memory_count;
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(SPACES_TABLE).expect("open spaces");
        t.insert(&space_key(brain_core::NamespaceId::SYSTEM.raw(), space_bytes), &meta)
            .expect("insert space");
    }
    wtxn.commit().expect("commit");
}

fn bruteforce_config(threshold: f32) -> SemanticRetrieverConfig {
    SemanticRetrieverConfig {
        similarity_threshold: threshold,
        top_k: 10,
        ..Default::default()
    }
}

fn filters_for(space: SpaceId, sessions: Vec<u64>) -> SemanticRetrieverConfig {
    let mut cfg = bruteforce_config(0.5);
    cfg.filters = SemanticFiltersConfigSlot(SemanticFilters {
        namespace_id: brain_core::NamespaceId::SYSTEM.raw(),
        space_ids: vec![space],
        session_ids: sessions,
        ..Default::default()
    });
    cfg
}

#[test]
fn bruteforce_returns_planted_space_exact_topk() {
    let (_dir, mut metadata) = fresh_metadata();

    let space_a = SpaceId::new();
    let space_b = SpaceId::new();

    // Space A (sparse, planted): three memories, distinct one-hot vectors.
    let id_match = plant_memory(&mut metadata, 10, 1, space_a, 0, 100);
    plant_memory(&mut metadata, 11, 1, space_a, 0, 101);
    plant_memory(&mut metadata, 12, 1, space_a, 0, 102);
    register_space(&mut metadata, space_a, 3);

    // Space B (decoy-heavy): many memories at the exact query vector. If
    // the lane leaked across spaces these would flood the result.
    for slot in 100..200u64 {
        plant_memory(&mut metadata, slot, 1, space_b, 0, 50);
    }
    register_space(&mut metadata, space_b, 100);

    // The mock arena knows every planted slot's vector. Space A's
    // slot 10 == query; slots 11/12 orthogonal; space B all == query.
    let mut vectors = std::collections::HashMap::new();
    vectors.insert((10u64, 1u32), one_hot(0));
    vectors.insert((11u64, 1u32), one_hot(1));
    vectors.insert((12u64, 1u32), one_hot(2));
    for slot in 100..200u64 {
        vectors.insert((slot, 1u32), one_hot(0));
    }
    let arena = MockArena { vectors };

    // HNSW is empty (never built), so any non-empty result proves the
    // brute-force lane ran, not the shared graph.
    let retriever = build_retriever(metadata);
    let cfg = filters_for(space_a, vec![]);
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");

    // Exact recall: only space A's slot-10 clears the 0.5 threshold
    // (cosine 1.0); slots 11/12 are orthogonal; space B never scanned.
    assert_eq!(result.len(), 1, "exactly the one on-topic space-A memory");
    assert_eq!(result[0].id, RankedItemId::Memory(id_match));
    assert!(result[0].score >= 0.99);
}

#[test]
fn bruteforce_respects_threshold_on_nonsense_query() {
    let (_dir, mut metadata) = fresh_metadata();
    let space_a = SpaceId::new();
    plant_memory(&mut metadata, 10, 1, space_a, 0, 100);
    plant_memory(&mut metadata, 11, 1, space_a, 0, 101);
    register_space(&mut metadata, space_a, 2);

    let mut vectors = std::collections::HashMap::new();
    vectors.insert((10u64, 1u32), one_hot(0));
    vectors.insert((11u64, 1u32), one_hot(1));
    let arena = MockArena { vectors };

    let retriever = build_retriever(metadata);
    let cfg = filters_for(space_a, vec![]);
    // Orthogonal to every planted vector → cosine 0 everywhere.
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(50))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");
    assert!(result.is_empty(), "no vector clears the 0.5 threshold");
}

#[test]
fn bruteforce_session_filter_applies_inline() {
    let (_dir, mut metadata) = fresh_metadata();
    let space_a = SpaceId::new();
    // Two memories, same vector, different sessions.
    let id_s1 = plant_memory(&mut metadata, 10, 1, space_a, 1, 100);
    plant_memory(&mut metadata, 11, 1, space_a, 2, 101);
    register_space(&mut metadata, space_a, 2);

    let mut vectors = std::collections::HashMap::new();
    vectors.insert((10u64, 1u32), one_hot(0));
    vectors.insert((11u64, 1u32), one_hot(0));
    let arena = MockArena { vectors };

    let retriever = build_retriever(metadata);
    let cfg = filters_for(space_a, vec![1]);
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");
    assert_eq!(result.len(), 1, "only session 1 survives the inline filter");
    assert_eq!(result[0].id, RankedItemId::Memory(id_s1));
}

#[test]
fn bruteforce_falls_through_when_space_too_large() {
    let (_dir, mut metadata) = fresh_metadata();
    let space_a = SpaceId::new();
    let id = plant_memory(&mut metadata, 10, 1, space_a, 0, 100);
    // memory_count above the cap → the lane must decline and fall through
    // to the (empty) shared HNSW path, yielding no results.
    register_space(&mut metadata, space_a, super::SPACE_BRUTEFORCE_MAX + 1);
    let _ = id;

    let mut vectors = std::collections::HashMap::new();
    vectors.insert((10u64, 1u32), one_hot(0));
    let arena = MockArena { vectors };

    let retriever = build_retriever(metadata);
    let cfg = filters_for(space_a, vec![]);
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");
    // Shared HNSW is empty, so fallthrough yields nothing — proving the
    // large-space gate declined the brute-force scan.
    assert!(result.is_empty(), "oversized space falls through to shared HNSW");
}
