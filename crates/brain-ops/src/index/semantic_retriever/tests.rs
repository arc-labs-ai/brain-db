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
        )
        .expect("retrieve");

    assert_eq!(result.len(), 1);
    assert!(result[0].score >= 0.95);
}
