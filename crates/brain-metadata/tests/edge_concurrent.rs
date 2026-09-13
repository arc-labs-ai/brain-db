//! Concurrent writer/reader tests for the unified edge table.
//!
//! redb serialises write transactions internally (single-writer-per-db
//! discipline at the file level). These tests verify that the
//! unified edge tables behave correctly under:
//!
//! - Two concurrent writers issuing 100 edges each from overlapping
//!   NodeRef sets — final state must contain all 200 edges, no
//!   duplicates, no corruption.
//! - High fan-in node with one continuous writer + four concurrent
//!   readers — readers via MVCC always see a consistent snapshot,
//!   writes never panic, no torn rows are observed.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use brain_core::{EdgeKind, EdgeKindRef, MemoryId, NodeRef};
use brain_metadata::tables::edge::{
    derived_by, link, origin, walk_outgoing, zero_disambiguator, EdgeData, EDGES_REVERSE_TABLE,
    EDGES_TABLE,
};
use brain_metadata::MetadataDb;
use redb::{ReadableDatabase, ReadableTable};

fn mid(slot: u64) -> MemoryId {
    MemoryId::pack(1, slot, 1)
}

fn mnode(slot: u64) -> NodeRef {
    NodeRef::Memory(mid(slot))
}

fn ed(weight: f32) -> EdgeData {
    EdgeData::new(
        weight,
        origin::EXPLICIT,
        derived_by::CLIENT,
        1_700_000_000_000_000_000,
    )
}

/// Two concurrent writers on the same shard never produce conflicting
/// `NodeRef` encoding for the same logical edge. redb serialises
/// `begin_write` per file, so the two writers run sequentially in
/// some order; the *combined* output must be exactly 200 edges with
/// no corruption.
///
/// We deliberately make the source-node sets overlap so the test
/// covers the case where both writers touch the same prefix range
/// (the highest-cardinality redb concurrency hazard).
#[test]
fn two_writers_overlapping_node_sets_no_corruption() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("md.redb");
    {
        // Initialise schema once on the main thread.
        let _ = MetadataDb::open(&path).unwrap();
    }
    // Reopen as a raw Arc<Database> we can share across threads (the
    // MetadataDb wrapper's `&mut self` for `write_txn` blocks
    // multi-threaded access; we go around it via the raw Database
    // precisely to *test* the concurrent path).
    let db = Arc::new(redb::Database::create(&path).unwrap());

    let writer = |db: Arc<redb::Database>, range_start: u64| {
        let mut count = 0;
        for slot in range_start..range_start + 100 {
            let wtxn = db.begin_write().unwrap();
            {
                let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
                let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
                // Writer A writes 0..100; writer B writes 50..150.
                // Slots 50..100 overlap. The *to* node also overlaps
                // (different shift) so reverse-table rows collide too
                // — except disambiguator differs (we encode the
                // writer-id into the disambiguator) so the unified
                // key tuple is unique per writer.
                let mut disamb = zero_disambiguator();
                disamb[0] = if range_start == 0 { 0xA0 } else { 0xB0 };
                disamb[15] = slot as u8;
                link(
                    &mut e,
                    &mut r,
                    mnode(slot),
                    EdgeKindRef::Builtin(EdgeKind::Caused),
                    mnode(slot + 1000),
                    disamb,
                    &ed(0.5),
                )
                .unwrap();
            }
            wtxn.commit().unwrap();
            count += 1;
        }
        count
    };

    let db_a = Arc::clone(&db);
    let db_b = Arc::clone(&db);
    let handle_a = thread::spawn(move || writer(db_a, 0));
    let handle_b = thread::spawn(move || writer(db_b, 50));

    let count_a = handle_a.join().unwrap();
    let count_b = handle_b.join().unwrap();
    assert_eq!(count_a, 100);
    assert_eq!(count_b, 100);

    let rtxn = db.begin_read().unwrap();
    let e = rtxn.open_table(EDGES_TABLE).unwrap();
    let r = rtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
    assert_eq!(e.iter().unwrap().count(), 200, "no duplicates, no losses");
    assert_eq!(r.iter().unwrap().count(), 200, "reverse mirrors forward");

    // Spot-check: every key decodes successfully (i.e., the bytes
    // aren't garbled by interleaved writes).
    for entry in e.iter().unwrap() {
        let (k, _v) = entry.unwrap();
        let decoded = brain_metadata::tables::edge::EdgeKey::decode(k.value());
        assert!(decoded.is_ok(), "edge key decode failed: {decoded:?}");
    }
}

/// High fan-in node with one continuous writer + four concurrent
/// reader threads. The writer continuously appends edges from a
/// single hot anchor; readers continuously call `walk_outgoing`
/// on the anchor. No panics, no torn reads (redb MVCC guarantees
/// snapshot isolation); every reader observation is internally
/// consistent.
#[test]
fn high_fanin_writer_vs_readers_consistent_snapshots() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("md.redb");
    {
        let _ = MetadataDb::open(&path).unwrap();
    }
    let db = Arc::new(redb::Database::create(&path).unwrap());

    // Seed an initial edge so readers see something non-empty before
    // the writer starts ramping.
    {
        let wtxn = db.begin_write().unwrap();
        {
            let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
            let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
            link(
                &mut e,
                &mut r,
                mnode(0),
                EdgeKindRef::Builtin(EdgeKind::Caused),
                mnode(1),
                zero_disambiguator(),
                &ed(1.0),
            )
            .unwrap();
        }
        wtxn.commit().unwrap();
    }

    let stop = Arc::new(AtomicBool::new(false));
    let writer_db = Arc::clone(&db);
    let writer_stop = Arc::clone(&stop);
    let writer = thread::spawn(move || {
        let mut slot: u64 = 2;
        while !writer_stop.load(Ordering::Relaxed) {
            let wtxn = writer_db.begin_write().unwrap();
            {
                let mut e = wtxn.open_table(EDGES_TABLE).unwrap();
                let mut r = wtxn.open_table(EDGES_REVERSE_TABLE).unwrap();
                link(
                    &mut e,
                    &mut r,
                    mnode(0),
                    EdgeKindRef::Builtin(EdgeKind::Caused),
                    mnode(slot),
                    zero_disambiguator(),
                    &ed(1.0),
                )
                .unwrap();
            }
            wtxn.commit().unwrap();
            slot += 1;
        }
        slot
    });

    let mut readers = Vec::new();
    for _ in 0..4 {
        let db_r = Arc::clone(&db);
        let stop_r = Arc::clone(&stop);
        readers.push(thread::spawn(move || {
            let mut max_seen = 0usize;
            let mut iterations = 0u64;
            let deadline = Instant::now() + Duration::from_millis(400);
            while Instant::now() < deadline && !stop_r.load(Ordering::Relaxed) {
                let rtxn = db_r.begin_read().unwrap();
                let rows = walk_outgoing(&rtxn, mnode(0), None).unwrap();
                // Within a single rtxn snapshot, every row decodes
                // and the target NodeRef is well-formed (no torn
                // bytes). We dereference each row's fields to force
                // any latent rkyv validation failures.
                for (kind, neighbour, _disamb, data) in &rows {
                    assert!(matches!(kind, EdgeKindRef::Builtin(EdgeKind::Caused)));
                    assert!(matches!(neighbour, NodeRef::Memory(_)));
                    let _ = data.weight;
                }
                max_seen = max_seen.max(rows.len());
                iterations += 1;
            }
            (max_seen, iterations)
        }));
    }

    // Let it run briefly, then stop.
    thread::sleep(Duration::from_millis(300));
    stop.store(true, Ordering::Relaxed);

    let final_slot = writer.join().unwrap();
    for (i, h) in readers.into_iter().enumerate() {
        let (max_seen, iters) = h.join().unwrap();
        assert!(iters > 0, "reader {i} made no progress");
        assert!(max_seen >= 1, "reader {i} never observed any edge");
    }

    // Sanity: final committed state has at least as many edges as the
    // writer thought it inserted (minus 1 for the seed-inclusive
    // semantics).
    let rtxn = db.begin_read().unwrap();
    let count = walk_outgoing(&rtxn, mnode(0), None).unwrap().len();
    // final_slot is one past the last slot the writer wrote; with the
    // seed edge mnode(0)→mnode(1), the count equals final_slot - 1.
    assert_eq!(
        count as u64,
        final_slot - 1,
        "final row count mismatch (writer wrote up to slot {final_slot})",
    );
}
