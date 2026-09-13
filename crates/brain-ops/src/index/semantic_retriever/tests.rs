//! Unit tests for `BrainSemanticRetriever`.

use std::sync::Arc;

use brain_core::{MemoryId, MemoryKind, SessionId, SpaceId};
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

/// Like `write_memory_row` but the row is soft-tombstoned (ACTIVE flag off).
/// Its HNSW node lingers, so the semantic lane must exclude it at source
/// unless `include_tombstoned` is set.
fn write_tombstoned_row(
    metadata: &mut MetadataDb,
    id: MemoryId,
    space: SpaceId,
    kind: MemoryKind,
    created_at_unix_ms: u64,
) {
    use brain_metadata::tables::memory::flags;
    let mut mem = MemoryMetadata::new_active(
        id,
        brain_core::NamespaceId::SYSTEM,
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
    mem.set_flag(flags::ACTIVE, false);
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

/// Empty `SpaceVectorSource`: the brute-force lane no longer resolves
/// vectors from the arena (its mmap holds only WAL-recovered vectors, so it
/// is empty for same-run encodes) — it reads the live redb artifact store
/// instead. This mock exists only to satisfy the shard-read-path gate
/// (`arena.is_some()`); returning `None` for every slot proves the lane
/// never depends on arena content.
struct EmptyArena;

impl SpaceVectorSource for EmptyArena {
    fn vector_at(&self, _slot: u64, _expected_version: u32) -> Option<[f32; VECTOR_DIM]> {
        None
    }
}

/// Plant a memory: its `MEMORIES_TABLE` row, its
/// `MEMORIES_BY_SPACE_TIMELINE_TABLE` key (which the brute-force lane
/// range-scans), and its live artifact vector (the by-id store the lane
/// resolves against). `tombstoned` flips the row's `ACTIVE` flag off.
/// Returns the `MemoryId`.
// Test fixture: the params map 1:1 onto the row/timeline/artifact fields a
// planted memory needs; bundling them into a struct would only obscure them.
#[allow(clippy::too_many_arguments)]
fn plant_memory(
    metadata: &mut MetadataDb,
    slot: u64,
    version: u32,
    space: SpaceId,
    session_id: u64,
    created_ms: u64,
    vector: [f32; VECTOR_DIM],
    tombstoned: bool,
) -> MemoryId {
    use brain_metadata::tables::memory::flags;
    let id = MemoryId::pack(0, slot, version);
    let ns = brain_core::NamespaceId::SYSTEM;
    let mut mem = MemoryMetadata::new_active(
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
    if tombstoned {
        mem.set_flag(flags::ACTIVE, false);
    }
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
        crate::memory_artifact::merge_memory_artifact(&wtxn, id.to_be_bytes(), |b| {
            b.vector = vector.to_vec();
        })
        .expect("plant artifact vector");
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
        t.insert(
            &space_key(brain_core::NamespaceId::SYSTEM.raw(), space_bytes),
            &meta,
        )
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
    // Slot 10 == query; slots 11/12 orthogonal.
    let id_match = plant_memory(&mut metadata, 10, 1, space_a, 0, 100, one_hot(0), false);
    plant_memory(&mut metadata, 11, 1, space_a, 0, 101, one_hot(1), false);
    plant_memory(&mut metadata, 12, 1, space_a, 0, 102, one_hot(2), false);
    register_space(&mut metadata, space_a, 3);

    // Space B (decoy-heavy): many memories at the exact query vector. If
    // the lane leaked across spaces these would flood the result.
    for slot in 100..200u64 {
        plant_memory(&mut metadata, slot, 1, space_b, 0, 50, one_hot(0), false);
    }
    register_space(&mut metadata, space_b, 100);

    // Empty arena (same-run: the mmap has nothing). The lane must resolve
    // vectors from the redb artifact store planted above, not the arena.
    let arena = EmptyArena;

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
    plant_memory(&mut metadata, 10, 1, space_a, 0, 100, one_hot(0), false);
    plant_memory(&mut metadata, 11, 1, space_a, 0, 101, one_hot(1), false);
    register_space(&mut metadata, space_a, 2);

    let arena = EmptyArena;
    let retriever = build_retriever(metadata);
    let cfg = filters_for(space_a, vec![]);
    // Orthogonal to every planted vector → cosine 0 everywhere. All
    // candidates resolve a vector, so the lane runs (no fallthrough) and
    // simply returns nothing above threshold.
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
    let id_s1 = plant_memory(&mut metadata, 10, 1, space_a, 1, 100, one_hot(0), false);
    plant_memory(&mut metadata, 11, 1, space_a, 2, 101, one_hot(0), false);
    register_space(&mut metadata, space_a, 2);

    let arena = EmptyArena;
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
    let id = plant_memory(&mut metadata, 10, 1, space_a, 0, 100, one_hot(0), false);
    // memory_count above the cap → the lane must decline and fall through
    // to the (empty) shared HNSW path, yielding no results.
    register_space(&mut metadata, space_a, super::SPACE_BRUTEFORCE_MAX + 1);
    let _ = id;

    let arena = EmptyArena;
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
    assert!(
        result.is_empty(),
        "oversized space falls through to shared HNSW"
    );
}

// ---------------------------------------------------------------------------
// Bug #6: same-run encodes resolve via the artifact store, and an
// unresolvable brute-force lane falls through to the shared HNSW.
// ---------------------------------------------------------------------------

#[test]
fn bruteforce_resolves_same_run_encodes_via_artifact_store() {
    // The regression: a fresh small-space tenant writes memories this run.
    // Those vectors live only in the redb artifact store (the arena mmap is
    // populated solely by WAL recovery on restart). A space-scoped RECALL
    // must still find them — the lane resolves from the artifact store, not
    // the empty arena.
    let (_dir, mut metadata) = fresh_metadata();
    let space = SpaceId::new();
    let id = plant_memory(&mut metadata, 10, 1, space, 0, 100, one_hot(0), false);
    register_space(&mut metadata, space, 1);

    let arena = EmptyArena; // arena has nothing — same-run encode
    let retriever = build_retriever(metadata);
    let cfg = filters_for(space, vec![]);
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1, "same-run encode must be recalled");
    assert_eq!(result[0].id, RankedItemId::Memory(id));
}

#[test]
fn bruteforce_falls_through_when_no_vector_resolves() {
    // Space is small and has a live candidate in the timeline, but its
    // artifact vector is absent (artifact store momentarily behind the
    // index). The lane must decline (return None) so the shared HNSW —
    // which carries the live vector — serves the query.
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();
    let id = MemoryId::pack(0, 10, 1);

    // Timeline row + metadata row, but DELIBERATELY no artifact vector.
    {
        let ns = brain_core::NamespaceId::SYSTEM;
        let mem = MemoryMetadata::new_active(
            id,
            ns,
            space,
            SessionId::from(0),
            id.slot(),
            id.version(),
            MemoryKind::Semantic,
            [0u8; 16],
            0.5,
            0,
            100 * 1_000_000,
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
                100 * 1_000_000,
                0,
                id.raw().to_be_bytes(),
            );
            tl.insert(&key[..], &()).expect("insert timeline");
        }
        wtxn.commit().expect("commit");
    }
    register_space(&mut metadata, space, 1);

    // The shared HNSW DOES carry the live vector.
    writer.insert(id, &one_hot(0)).expect("insert hnsw");

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    let arena = EmptyArena;
    let cfg = filters_for(space, vec![]);
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");

    // Fallthrough reached the shared HNSW, which returned the live hit.
    assert_eq!(result.len(), 1, "fallthrough must serve via shared HNSW");
    assert_eq!(result[0].id, RankedItemId::Memory(id));
}

#[test]
fn bruteforce_excludes_tombstoned_but_admits_when_requested() {
    // Bug #7 in the brute-force lane: a soft-tombstoned memory must not
    // surface by default, but must when include_tombstoned is set.
    let (_dir, mut metadata) = fresh_metadata();
    let space = SpaceId::new();
    let live = plant_memory(&mut metadata, 10, 1, space, 0, 100, one_hot(0), false);
    let dead = plant_memory(&mut metadata, 11, 1, space, 0, 101, one_hot(0), true);
    register_space(&mut metadata, space, 2);

    let arena = EmptyArena;
    let retriever = build_retriever(metadata);

    // Default: tombstoned excluded.
    let cfg = filters_for(space, vec![]);
    let result = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg,
            Some(&arena),
        )
        .expect("retrieve");
    let ids: Vec<RankedItemId> = result.iter().map(|r| r.id).collect();
    assert!(
        ids.contains(&RankedItemId::Memory(live)),
        "live must surface"
    );
    assert!(
        !ids.contains(&RankedItemId::Memory(dead)),
        "tombstoned must be excluded by default"
    );

    // include_tombstoned: both surface.
    let mut cfg2 = filters_for(space, vec![]);
    let SemanticFiltersConfigSlot(ref mut f) = cfg2.filters;
    f.include_tombstoned = true;
    let result2 = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg2,
            Some(&arena),
        )
        .expect("retrieve");
    let ids2: Vec<RankedItemId> = result2.iter().map(|r| r.id).collect();
    assert!(ids2.contains(&RankedItemId::Memory(live)));
    assert!(
        ids2.contains(&RankedItemId::Memory(dead)),
        "tombstoned admitted when requested"
    );
}

// ---------------------------------------------------------------------------
// Bug #7: the shared-HNSW lane excludes tombstoned rows at source, so live
// matches sitting below tombstoned candidates in the ef window are not
// starved. With include_tombstoned=true the tombstoned rows are admitted.
// ---------------------------------------------------------------------------

#[test]
fn hnsw_lane_excludes_tombstoned_and_preserves_live_recall() {
    let (_dir, mut metadata) = fresh_metadata();
    let (reader, mut writer) = SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw");
    let space = SpaceId::new();

    // 3 live + 3 tombstoned, all at the exact query vector so cosine alone
    // cannot separate them — only the tombstone gate can. top_k=3 < M(=3):
    // if the tombstoned rows were admitted they could fill the whole window
    // and starve the live rows.
    let mut live_ids = Vec::new();
    for slot in 1..=3u64 {
        let id = MemoryId::pack(0, slot, 1);
        writer.insert(id, &one_hot(0)).expect("insert live");
        write_memory_row(&mut metadata, id, space, MemoryKind::Episodic, slot);
        live_ids.push(id);
    }
    let mut dead_ids = Vec::new();
    for slot in 10..=12u64 {
        let id = MemoryId::pack(0, slot, 1);
        writer.insert(id, &one_hot(0)).expect("insert dead");
        write_tombstoned_row(&mut metadata, id, space, MemoryKind::Episodic, slot);
        dead_ids.push(id);
    }

    let embedder: Arc<dyn Dispatcher> = Arc::new(FixedDispatcher {
        vector: one_hot(0),
        fingerprint: [0u8; 16],
    });
    let retriever = BrainSemanticRetriever::new(embedder, reader, None, Arc::new(metadata));

    // Default RECALL: tombstoned excluded. Use a namespace-only filter (no
    // space scope) so the shared-HNSW lane runs, not the brute-force lane.
    let cfg = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(SemanticFilters {
            namespace_id: brain_core::NamespaceId::SYSTEM.raw(),
            ..Default::default()
        }),
        top_k: 3,
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
    assert_eq!(ids.len(), 3, "top_k live rows, none starved by tombstoned");
    for id in &live_ids {
        assert!(ids.contains(id), "live {id:?} must be present");
    }
    for id in &dead_ids {
        assert!(!ids.contains(id), "tombstoned {id:?} must be excluded");
    }

    // include_tombstoned: tombstoned admitted.
    let cfg2 = SemanticRetrieverConfig {
        filters: SemanticFiltersConfigSlot(SemanticFilters {
            namespace_id: brain_core::NamespaceId::SYSTEM.raw(),
            include_tombstoned: true,
            ..Default::default()
        }),
        top_k: 10,
        ..Default::default()
    };
    let result2 = retriever
        .retrieve(
            &SemanticQuery::Vector(Box::new(one_hot(0))),
            SemanticScope::Memory,
            &cfg2,
            None,
        )
        .expect("retrieve");
    let ids2: Vec<MemoryId> = result2
        .iter()
        .filter_map(|r| match r.id {
            RankedItemId::Memory(id) => Some(id),
            _ => None,
        })
        .collect();
    for id in dead_ids.iter().chain(live_ids.iter()) {
        assert!(ids2.contains(id), "{id:?} admitted with include_tombstoned");
    }
}

// ---------------------------------------------------------------------------
// vector_for — resolve the stored embedding by id instead of re-embedding on
// the read hot path. The entity-graph walk calls this per candidate; a
// re-embed here burns the BGE model needlessly.
// ---------------------------------------------------------------------------

/// A vector the fixed test embedder never returns (it returns `one_hot(0)`),
/// so an exact match proves the value came from the by-id artifact store.
fn stored_vec() -> [f32; VECTOR_DIM] {
    let mut v = [0.0f32; VECTOR_DIM];
    for (i, slot) in v.iter_mut().enumerate() {
        *slot = ((i % 7) as f32).mul_add(0.125, 0.5);
    }
    v
}

#[test]
fn vector_for_resolves_stored_vector_by_id_not_by_embedding() {
    let (_dir, metadata) = fresh_metadata();
    let id = MemoryId::pack(0, 1, 0);
    // Persist the exact ENCODE-time vector by id (the artifact bundle, which
    // `get_artifact_vector` resolves). No TEXTS row is written, so the only
    // way to answer is the by-id lookup — a re-embed would have nothing to
    // read anyway, and would return `one_hot(0)` if it fell through.
    {
        let wtxn = metadata.write_txn().expect("wtxn");
        crate::memory_artifact::merge_memory_artifact(&wtxn, id.to_be_bytes(), |b| {
            b.vector = stored_vec().to_vec();
        })
        .expect("store artifact vector");
        wtxn.commit().expect("commit");
    }
    let retriever = build_retriever(metadata);
    let got = retriever.vector_for(id).expect("vector resolves by id");
    assert_eq!(
        got,
        stored_vec(),
        "vector_for must return the stored vector verbatim",
    );
    assert_ne!(
        got,
        one_hot(0),
        "resolving by id must not fall through to the embedder",
    );
}

#[test]
fn vector_for_falls_back_to_embedding_when_artifact_absent() {
    use brain_metadata::tables::text::TEXTS_TABLE;
    let (_dir, metadata) = fresh_metadata();
    let id = MemoryId::pack(0, 2, 0);
    // No artifact vector for this id — only a TEXTS row. This is the
    // fresh-this-run miss: the by-id store has nothing yet, so vector_for
    // reconstructs from the stored text via the embedder.
    {
        let wtxn = metadata.write_txn().expect("wtxn");
        {
            let mut t = wtxn.open_table(TEXTS_TABLE).expect("open texts");
            t.insert(&id.to_be_bytes(), b"some passage".as_slice())
                .expect("insert text");
        }
        wtxn.commit().expect("commit");
    }
    let retriever = build_retriever(metadata);
    let got = retriever
        .vector_for(id)
        .expect("vector resolves via fallback");
    assert_eq!(
        got,
        one_hot(0),
        "fallback path embeds the stored text (fixed embedder returns one_hot(0))",
    );
}

#[test]
fn vector_for_returns_none_when_neither_artifact_nor_text_present() {
    let (_dir, metadata) = fresh_metadata();
    let retriever = build_retriever(metadata);
    // An id with no artifact vector and no TEXTS row: nothing to resolve or
    // embed, so the candidate keeps its structural graph score (caller's
    // decision) rather than a fabricated vector.
    assert!(
        retriever.vector_for(MemoryId::pack(0, 9, 0)).is_none(),
        "no by-id vector and no text ⇒ None",
    );
}
