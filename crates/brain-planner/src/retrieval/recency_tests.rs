//! Unit tests for the event-time recency boost.

use brain_core::{AgentId, ContextId, MemoryId, MemoryKind};
use brain_index::RankedItemId;
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::MetadataDb;
use tempfile::TempDir;

use super::{apply_recency_boost, RECENCY_HALF_LIFE_DAYS};
use crate::retrieval::fusion::FusedItem;

const NANOS_PER_DAY: u64 = 86_400 * 1_000_000_000;

fn fresh() -> (TempDir, MetadataDb) {
    let dir = TempDir::new().expect("tempdir");
    let metadata = MetadataDb::open(dir.path().join("metadata.redb")).expect("open");
    (dir, metadata)
}

/// Insert a memory row carrying an explicit event time.
fn put_memory(metadata: &MetadataDb, id: MemoryId, occurred_at: Option<u64>, created_at: u64) {
    let row = MemoryMetadata::new_active(
        id,
        AgentId::new(),
        ContextId::from(0),
        id.slot(),
        id.version(),
        MemoryKind::Episodic,
        [0u8; 16],
        0.5,
        0,
        created_at,
    )
    .with_occurred_at(occurred_at);
    let wtxn = metadata.write_txn().expect("wtxn");
    {
        let mut t = wtxn.open_table(MEMORIES_TABLE).expect("open");
        t.insert(&id.raw().to_be_bytes(), &row).expect("insert");
    }
    wtxn.commit().expect("commit");
}

/// A fused item with an explicit starting score so we can reason about
/// the additive boost precisely.
fn fused(id: MemoryId, score: f64) -> FusedItem {
    FusedItem {
        id: RankedItemId::Memory(id),
        fused_score: score,
        contributing: Vec::new(),
        rerank_score: None,
    }
}

#[test]
fn recent_memory_outranks_older_after_boost() {
    let (_dir, metadata) = fresh();
    let now = 1_900_000_000 * 1_000_000_000_u64;

    let recent = MemoryId::pack(0, 1, 0);
    let old = MemoryId::pack(0, 2, 0);
    // `old` starts marginally ahead on pure relevance; recency should
    // flip the order once the boost is applied.
    put_memory(&metadata, recent, Some(now - NANOS_PER_DAY), now); // ~1 day old
    put_memory(&metadata, old, Some(now - 400 * NANOS_PER_DAY), now); // >1 year old

    let mut items = vec![fused(old, 0.0170), fused(recent, 0.0164)];
    apply_recency_boost(&mut items, &metadata, now, 0.5, 60).expect("boost");

    assert_eq!(
        items[0].id,
        RankedItemId::Memory(recent),
        "the day-old memory must outrank the year-old one once recency is folded in",
    );
    assert!(
        items[1].id == RankedItemId::Memory(old),
        "the older memory drops to second",
    );
}

#[test]
fn occurred_at_takes_precedence_over_created_at() {
    let (_dir, metadata) = fresh();
    let now = 1_900_000_000 * 1_000_000_000_u64;

    // Both rows were *written* now, but one happened long ago. The
    // event time, not the write time, must drive the decay.
    let written_now_happened_old = MemoryId::pack(0, 1, 0);
    let written_now_happened_now = MemoryId::pack(0, 2, 0);
    put_memory(
        &metadata,
        written_now_happened_old,
        Some(now - 500 * NANOS_PER_DAY),
        now,
    );
    put_memory(&metadata, written_now_happened_now, Some(now), now);

    let mut items = vec![
        fused(written_now_happened_old, 0.0164),
        fused(written_now_happened_now, 0.0164),
    ];
    apply_recency_boost(&mut items, &metadata, now, 0.5, 60).expect("boost");

    assert_eq!(
        items[0].id,
        RankedItemId::Memory(written_now_happened_now),
        "event-time (occurred_at), not write-time, drives recency",
    );
}

#[test]
fn falls_back_to_created_at_when_occurred_at_absent() {
    let (_dir, metadata) = fresh();
    let now = 1_900_000_000 * 1_000_000_000_u64;

    let id = MemoryId::pack(0, 1, 0);
    put_memory(&metadata, id, None, now); // no event time → use created_at = now

    let mut items = vec![fused(id, 0.0)];
    apply_recency_boost(&mut items, &metadata, now, 0.5, 60).expect("boost");

    // created_at == now ⇒ decay ≈ 1 ⇒ boost ≈ temporal_weight / (k+1).
    let expected = 0.5 * (1.0 / 61.0);
    assert!(
        (items[0].fused_score - expected).abs() < 1e-9,
        "absent occurred_at falls back to created_at; got {}",
        items[0].fused_score,
    );
}

#[test]
fn half_life_halves_the_boost() {
    let (_dir, metadata) = fresh();
    let now = 1_900_000_000 * 1_000_000_000_u64;

    let id = MemoryId::pack(0, 1, 0);
    let one_half_life_ago = now - (RECENCY_HALF_LIFE_DAYS as u64) * NANOS_PER_DAY;
    put_memory(&metadata, id, Some(one_half_life_ago), now);

    let mut items = vec![fused(id, 0.0)];
    apply_recency_boost(&mut items, &metadata, now, 0.5, 60).expect("boost");

    // One half-life old ⇒ decay ≈ 0.5 ⇒ boost ≈ 0.5 · (1/61) · 0.5.
    let expected = 0.5 * (1.0 / 61.0) * 0.5;
    assert!(
        (items[0].fused_score - expected).abs() < 1e-6,
        "a one-half-life-old memory gets half the freshness boost; got {}",
        items[0].fused_score,
    );
}

#[test]
fn zero_weight_is_a_noop() {
    let (_dir, metadata) = fresh();
    let now = 1_900_000_000 * 1_000_000_000_u64;
    let id = MemoryId::pack(0, 1, 0);
    put_memory(&metadata, id, Some(now), now);

    let mut items = vec![fused(id, 0.0164)];
    apply_recency_boost(&mut items, &metadata, now, 0.0, 60).expect("boost");

    assert!(
        (items[0].fused_score - 0.0164).abs() < 1e-12,
        "temporal_weight = 0 must not change any score",
    );
}

#[test]
fn future_dated_event_saturates_at_full_freshness() {
    let (_dir, metadata) = fresh();
    let now = 1_900_000_000 * 1_000_000_000_u64;
    let id = MemoryId::pack(0, 1, 0);
    // Event time after the reference point (clock skew / scheduled event).
    put_memory(&metadata, id, Some(now + 10 * NANOS_PER_DAY), now);

    let mut items = vec![fused(id, 0.0)];
    apply_recency_boost(&mut items, &metadata, now, 0.5, 60).expect("boost");

    let full = 0.5 * (1.0 / 61.0);
    assert!(
        (items[0].fused_score - full).abs() < 1e-9,
        "a future-dated event saturates at full freshness, not >1",
    );
}
