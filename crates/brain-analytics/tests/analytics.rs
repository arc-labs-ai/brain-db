//! Snapshot export + Tier 2 queries checked against a brute-force
//! computation over the same rows.

#![cfg(target_os = "linux")]

use std::sync::Arc;
use std::time::Duration;

use brain_analytics::{Analytics, AnalyticsConfig, ScanGroup, ScanQuery};
use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
use brain_metadata::tables::agg::{put_memory, status_of};
use brain_metadata::tables::memory::{
    flags, space_timeline_key, MemoryMetadata, MEMORIES_BY_SPACE_TIMELINE_TABLE, MEMORIES_TABLE,
};
use brain_metadata::MetadataDb;
use redb::ReadableTable;

const DAY: u64 = 86_400_000_000_000;
const T0: u64 = 1_700_000_000_000_000_000;

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, n: u64) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1);
        (self.0 >> 33) % n
    }
}

fn space(b: u8) -> [u8; 16] {
    let mut s = [0u8; 16];
    s[15] = b;
    s
}

/// 3 000 memories over 2 namespaces x 3 spaces, 3 kinds, ~10 days, some
/// tombstoned, some hard-forgotten, some missing from the timeline index
/// (as WAL recovery leaves them).
fn seed(db: &MetadataDb) -> Vec<MemoryMetadata> {
    let mut rng = Lcg(7);
    let kinds = [
        MemoryKind::Episodic,
        MemoryKind::Semantic,
        MemoryKind::Consolidated,
    ];
    let mut rows = Vec::new();
    let wtxn = db.write_txn().unwrap();
    {
        let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
        let mut tl = wtxn.open_table(MEMORIES_BY_SPACE_TIMELINE_TABLE).unwrap();
        for slot in 0..3_000u64 {
            let mut r = MemoryMetadata::new_active(
                MemoryId::pack(0, slot, 1),
                NamespaceId::from(rng.next(2) as u32),
                SpaceId::from(space(rng.next(3) as u8)),
                SessionId(rng.next(4)),
                slot,
                1,
                kinds[rng.next(3) as usize],
                [0; 16],
                rng.next(1000) as f32 / 1000.0,
                10,
                T0 + rng.next(10 * DAY),
            );
            r.access_count = rng.next(20) as u32;
            match rng.next(10) {
                0 => r.flags &= !flags::ACTIVE,
                1 => r.flags = (r.flags & !flags::ACTIVE) | flags::HARD_FORGOTTEN,
                _ => {}
            }
            put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
            // Writers index active rows only; skip some to mimic recovery.
            if r.flags & flags::ACTIVE != 0 && rng.next(20) != 0 {
                let k = space_timeline_key(
                    r.namespace_id,
                    r.space_id_bytes,
                    r.created_at_unix_nanos,
                    r.session_id,
                    r.memory_id_bytes,
                );
                tl.insert(k.as_slice(), ()).unwrap();
            }
            rows.push(r);
        }
    }
    wtxn.commit().unwrap();
    rows
}

fn brute(rows: &[MemoryMetadata], q: &ScanQuery) -> Vec<ScanGroup> {
    use std::collections::BTreeMap;
    let statuses = if q.statuses.is_empty() {
        vec![0]
    } else {
        q.statuses.clone()
    };
    type GroupKey = (Option<u8>, Option<u8>, Option<u64>);
    let mut m: BTreeMap<GroupKey, ScanGroup> = BTreeMap::new();
    for r in rows {
        let created = r.created_at_unix_nanos;
        let ok = r.namespace_id == q.namespace_id
            && r.space_id_bytes == q.space_id
            && statuses.contains(&status_of(r))
            && (q.kinds.is_empty() || q.kinds.contains(&r.kind))
            && q.salience_min
                .is_none_or(|v| r.salience_at(q.at_unix_nanos) >= v)
            && q.salience_max
                .is_none_or(|v| r.salience_at(q.at_unix_nanos) <= v)
            && q.created_from_unix_nanos.is_none_or(|v| created >= v)
            && q.created_to_unix_nanos.is_none_or(|v| created <= v);
        if !ok {
            continue;
        }
        let key = (
            q.group_by_kind.then_some(r.kind),
            q.group_by_status.then_some(status_of(r)),
            q.group_by_day.then_some(created / DAY * DAY),
        );
        let g = m.entry(key).or_insert(ScanGroup {
            kind: key.0,
            status: key.1,
            day_start_unix_nanos: key.2,
            count: 0,
            salience_sum: 0.0,
            salience_min: None,
            salience_max: None,
            access_sum: 0,
        });
        let s = f64::from(r.salience_at(q.at_unix_nanos));
        g.count += 1;
        g.salience_sum += s;
        g.salience_min = Some(g.salience_min.map_or(s, |v| v.min(s)));
        g.salience_max = Some(g.salience_max.map_or(s, |v| v.max(s)));
        g.access_sum += u64::from(r.access_count);
    }
    if m.is_empty() && !q.group_by_kind && !q.group_by_status && !q.group_by_day {
        return vec![ScanGroup {
            kind: None,
            status: None,
            day_start_unix_nanos: None,
            count: 0,
            salience_sum: 0.0,
            salience_min: None,
            salience_max: None,
            access_sum: 0,
        }];
    }
    m.into_values().collect()
}

fn assert_same(got: &[ScanGroup], want: &[ScanGroup], q: &ScanQuery) {
    assert_eq!(got.len(), want.len(), "group count for {q:?}");
    for (g, w) in got.iter().zip(want) {
        assert_eq!(
            (
                g.kind,
                g.status,
                g.day_start_unix_nanos,
                g.count,
                g.access_sum
            ),
            (
                w.kind,
                w.status,
                w.day_start_unix_nanos,
                w.count,
                w.access_sum
            ),
            "{q:?}"
        );
        assert!((g.salience_sum - w.salience_sum).abs() < 1e-3, "{q:?}");
        // Rows evaluate salience in f32, the snapshot in f64.
        let close = |a: Option<f64>, b: Option<f64>| match (a, b) {
            (Some(a), Some(b)) => (a - b).abs() < 1e-6,
            (a, b) => a == b,
        };
        assert!(close(g.salience_min, w.salience_min), "{q:?}");
        assert!(close(g.salience_max, w.salience_max), "{q:?}");
    }
}

fn start(db: Arc<MetadataDb>, dir: &std::path::Path) -> Analytics {
    Analytics::start(
        db,
        dir,
        AnalyticsConfig {
            export_interval: Duration::from_secs(3600),
            memory_limit_bytes: 64 << 20,
            ..AnalyticsConfig::default()
        },
    )
    .unwrap()
}

#[test]
fn snapshot_has_every_memory_once_and_queries_match_brute_force() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    let rows = seed(&db);
    let a = start(db.clone(), &dir.path().join("analytics"));
    let snap = a.export_now().unwrap();
    assert_eq!(
        snap.rows,
        rows.len() as u64,
        "each memory exported exactly once"
    );

    let mut rng = Lcg(99);
    for i in 0..60 {
        let from = (rng.next(2) == 0).then(|| T0 + rng.next(5) * DAY);
        let q = ScanQuery {
            namespace_id: rng.next(2) as u32,
            space_id: space(rng.next(3) as u8),
            kinds: if rng.next(2) == 0 {
                vec![]
            } else {
                vec![rng.next(3) as u8]
            },
            statuses: match rng.next(3) {
                0 => vec![],
                1 => vec![0, 1],
                _ => vec![0, 1, 2],
            },
            salience_min: (rng.next(2) == 0).then(|| rng.next(500) as f32 / 1000.0),
            salience_max: (rng.next(3) == 0).then(|| 0.5 + rng.next(500) as f32 / 1000.0),
            created_from_unix_nanos: from,
            created_to_unix_nanos: from.map(|f| f + rng.next(4) * DAY),
            group_by_kind: i % 2 == 0,
            group_by_status: i % 3 == 0,
            group_by_day: i % 4 == 0,
            // Salience decays: evaluate anywhere from just after the last
            // creation to well past most rows' floor. (Before a row's
            // anchor, aggregates extrapolate while a row read clamps.)
            at_unix_nanos: T0 + (10 + rng.next(400)) * DAY,
        };
        let got = futures_lite::future::block_on(a.query(q.clone())).unwrap();
        // The background loop may have exported again since; same data.
        assert!(got.snapshot_taken_at_unix_nanos >= snap.taken_at_unix_nanos);
        assert_same(&got.groups, &brute(&rows, &q), &q);
    }
}

#[test]
fn restart_serves_the_previous_snapshot_and_prunes_old_ones() {
    let dir = tempfile::tempdir().unwrap();
    let adir = dir.path().join("analytics");
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    seed(&db);
    let first = start(db.clone(), &adir);
    let s1 = first.export_now().unwrap();
    first.export_now().unwrap();
    let s3 = first.export_now().unwrap();
    drop(first);

    let files = std::fs::read_dir(&adir).unwrap().count();
    // Two snapshots of BUCKETS files each, plus the spill directory.
    assert!(
        files <= 2 * brain_analytics::export::BUCKETS + 1,
        "old snapshots pruned, found {files}"
    );
    assert!(!s1.path.exists(), "oldest snapshot deleted");

    let again = start(db.clone(), &adir);
    let cur = again
        .current_snapshot()
        .expect("picked up existing snapshot");
    assert!(cur.taken_at_unix_nanos >= s3.taken_at_unix_nanos);
    assert_eq!(cur.rows, s3.rows);
}

#[test]
fn rejects_a_query_timeout_that_outlives_the_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    let err = Analytics::start(
        db,
        dir.path().join("analytics"),
        AnalyticsConfig {
            export_interval: Duration::from_secs(10),
            query_timeout: Duration::from_secs(10),
            ..AnalyticsConfig::default()
        },
    )
    .err()
    .expect("timeout >= interval must be refused");
    assert!(err.to_string().contains("query_timeout"), "{err}");
}

/// Partitioned export over many partitions still emits every memory once
/// and spills go under the analytics dir, not the OS temp dir.
#[test]
fn many_partitions_export_each_row_once() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    let rows = seed(&db);
    let adir = dir.path().join("analytics");
    let a = Analytics::start(
        db.clone(),
        &adir,
        AnalyticsConfig {
            threads: 7,
            ..AnalyticsConfig::default()
        },
    )
    .unwrap();
    assert_eq!(a.export_now().unwrap().rows, rows.len() as u64);
    assert!(adir.join("spill").is_dir());
    let q = ScanQuery {
        namespace_id: 1,
        space_id: space(2),
        statuses: vec![0, 1, 2],
        group_by_kind: true,
        ..ScanQuery::default()
    };
    let got = futures_lite::future::block_on(a.query(q.clone())).unwrap();
    // `at_unix_nanos: 0` evaluates at the time of the query.
    let at_now = ScanQuery {
        at_unix_nanos: brain_metadata::salience::now_unix_nanos(),
        ..q.clone()
    };
    assert_same(&got.groups, &brute(&rows, &at_now), &q);
}

/// A snapshot exported "before" the previous one by the wall clock still
/// sorts newest, and pruning never deletes the current snapshot.
#[test]
fn snapshot_names_stay_ordered_when_the_clock_steps_back() {
    let dir = tempfile::tempdir().unwrap();
    let adir = dir.path().join("analytics");
    std::fs::create_dir_all(&adir).unwrap();
    let db = MetadataDb::open(dir.path().join("meta.redb")).unwrap();
    seed(&db);
    // Previous snapshot "from the future" (clock later stepped back).
    let future = 4_000_000_000_000_000_000u64;
    let first = brain_analytics::export_snapshot(&db, &adir, 2, future, &|| false).unwrap();
    assert_eq!(first.taken_at_unix_nanos, future + 1);
    let second =
        brain_analytics::export_snapshot(&db, &adir, 2, first.taken_at_unix_nanos, &|| false)
            .unwrap();
    assert!(second.taken_at_unix_nanos > first.taken_at_unix_nanos);
    assert!(second.path > first.path, "names sort in export order");
}

/// A damaged newest snapshot is removed at start instead of failing it.
/// An older one is not served instead (the shard may have pruned the
/// change log it would need); the export loop takes a fresh one at once.
#[test]
fn unreadable_snapshot_does_not_block_start() {
    let dir = tempfile::tempdir().unwrap();
    let adir = dir.path().join("analytics");
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    seed(&db);
    let good = start(db.clone(), &adir).export_now().unwrap();
    // A newer snapshot with one damaged (and every other) bucket file
    // missing.
    let bad = adir.join("memories-18000000000000000000-b00.parquet");
    std::fs::write(&bad, b"not parquet").unwrap();

    let a = start(db.clone(), &adir);
    assert!(!bad.exists(), "damaged snapshot removed");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    let cur = loop {
        if let Some(c) = a.current_snapshot() {
            break c;
        }
        assert!(std::time::Instant::now() < deadline, "no fresh export");
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_ne!(cur.path, bad);
    assert!(
        cur.taken_at_unix_nanos > good.taken_at_unix_nanos,
        "a new full export"
    );
}

/// Writes after a full snapshot reach queries through a small delta:
/// updates, deletes and new memories, with the base untouched. A delta
/// over the size limit becomes a new base instead, and the shard may then
/// prune the change log up to it.
#[test]
fn deltas_carry_changes_and_compact_into_a_new_base() {
    use brain_metadata::tables::agg::remove_memory;
    let dir = tempfile::tempdir().unwrap();
    let adir = dir.path().join("analytics");
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    seed(&db);
    let a = start(db.clone(), &adir);
    let base = a.export_now().unwrap();
    assert_eq!(
        a.prune_upto().load(std::sync::atomic::Ordering::Relaxed),
        base.change_seq
    );

    // Update 40, delete 15, add 25.
    let wtxn = db.write_txn().unwrap();
    {
        let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for slot in 0..40u64 {
            let id = MemoryId::pack(0, slot * 7, 1).to_be_bytes();
            let mut r = mem.get(&id).unwrap().unwrap().value();
            r.access_count += 100;
            r.kind = (r.kind + 1) % 3;
            put_memory(&wtxn, &mut mem, id, &r).unwrap();
        }
        for slot in 0..15u64 {
            let id = MemoryId::pack(0, 1_000 + slot * 11, 1).to_be_bytes();
            remove_memory(&wtxn, &mut mem, id).unwrap();
        }
        for slot in 0..25u64 {
            let r = MemoryMetadata::new_active(
                MemoryId::pack(0, 10_000 + slot, 1),
                NamespaceId::from((slot % 2) as u32),
                SpaceId::from(space((slot % 3) as u8)),
                SessionId(1),
                10_000 + slot,
                1,
                MemoryKind::Semantic,
                [0; 16],
                0.7,
                10,
                T0 + slot * DAY,
            );
            put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
        }
    }
    wtxn.commit().unwrap();

    let delta = a.refresh_now().unwrap();
    assert_eq!(delta.rows, 80, "only the changed memories");
    let cur = a.current().unwrap();
    assert_eq!(cur.base.path, base.path, "base untouched");
    assert_eq!(cur.delta.as_ref().map(|d| &d.path), Some(&delta.path));

    let rows: Vec<MemoryMetadata> = {
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        t.iter().unwrap().map(|e| e.unwrap().1.value()).collect()
    };
    for ns in 0..2u32 {
        for sp in 0..3u8 {
            let q = ScanQuery {
                namespace_id: ns,
                space_id: space(sp),
                statuses: vec![0, 1, 2],
                group_by_kind: true,
                group_by_status: true,
                // After every row's anchor (see salience::forward_scale).
                at_unix_nanos: T0 + 40 * DAY,
                ..ScanQuery::default()
            };
            let got = futures_lite::future::block_on(a.query(q.clone())).unwrap();
            assert_eq!(got.snapshot_taken_at_unix_nanos, delta.taken_at_unix_nanos);
            assert_same(&got.groups, &brute(&rows, &q), &q);
        }
    }

    // A delta past DELTA_MIN_ROWS (base is 3 000 rows) becomes a new base.
    let wtxn = db.write_txn().unwrap();
    {
        let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
        for slot in 0..(brain_analytics::DELTA_MIN_ROWS as u64 + 1) {
            let r = MemoryMetadata::new_active(
                MemoryId::pack(0, 100_000 + slot, 1),
                NamespaceId::from(0),
                SpaceId::from(space(0)),
                SessionId(1),
                100_000 + slot,
                1,
                MemoryKind::Episodic,
                [0; 16],
                0.4,
                10,
                T0,
            );
            put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
        }
    }
    wtxn.commit().unwrap();
    let next = a.refresh_now().unwrap();
    let cur = a.current().unwrap();
    assert_eq!(cur.base.path, next.path, "compacted into a new base");
    assert!(cur.delta.is_none());
    // Compaction merged the old snapshot and the delta from Parquet alone:
    // every live memory once, and queries still match the rows.
    let rows: Vec<MemoryMetadata> = {
        let rtxn = db.read_txn().unwrap();
        let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
        t.iter().unwrap().map(|e| e.unwrap().1.value()).collect()
    };
    assert_eq!(next.rows, rows.len() as u64);
    for ns in 0..2u32 {
        for sp in 0..3u8 {
            let q = ScanQuery {
                namespace_id: ns,
                space_id: space(sp),
                statuses: vec![0, 1, 2],
                group_by_kind: true,
                at_unix_nanos: T0 + 40 * DAY,
                ..ScanQuery::default()
            };
            let got = futures_lite::future::block_on(a.query(q.clone())).unwrap();
            assert_same(&got.groups, &brute(&rows, &q), &q);
        }
    }
    assert!(next.change_seq > base.change_seq);
    assert_eq!(
        a.prune_upto().load(std::sync::atomic::Ordering::Relaxed),
        next.change_seq
    );
}

/// Analytics does not keep the shard's DB alive: once the shard drops it,
/// exports report Stopped.
#[test]
fn exports_stop_once_the_shard_drops_its_db() {
    let dir = tempfile::tempdir().unwrap();
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    let a = start(db.clone(), &dir.path().join("analytics"));
    drop(db);
    assert!(matches!(
        a.export_now(),
        Err(brain_analytics::AnalyticsError::Stopped)
    ));
}

/// A shard stopping mid-export ends the export promptly (no file left) and
/// releases the DB before the shard's lifeline shutdown returns.
#[test]
fn export_stops_when_the_shard_lifeline_ends() {
    let dir = tempfile::tempdir().unwrap();
    let adir = dir.path().join("analytics");
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    seed(&db);
    let owner = brain_metadata::LifelineOwner::new();
    let a = Analytics::start_with_lifeline(
        db.clone(),
        &adir,
        AnalyticsConfig::default(),
        Some(owner.lifeline()),
    )
    .unwrap();
    assert!(
        owner.shutdown(Duration::from_secs(30)),
        "export left the DB"
    );
    assert!(matches!(
        a.export_now(),
        Err(brain_analytics::AnalyticsError::Stopped)
    ));
    let leftovers: Vec<_> = std::fs::read_dir(&adir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
        .collect();
    assert!(leftovers.is_empty(), "no temp file left: {leftovers:?}");
    // Only the shard's own handle remains.
    assert_eq!(Arc::strong_count(&db), 1);
}

/// A delta with only updates, then one with only deletions (each leaves
/// one side of the delta empty), still refresh and query correctly.
#[test]
fn deltas_with_no_deletions_or_no_live_rows() {
    use brain_metadata::tables::agg::remove_memory;
    let dir = tempfile::tempdir().unwrap();
    let adir = dir.path().join("analytics");
    let db = Arc::new(MetadataDb::open(dir.path().join("meta.redb")).unwrap());
    seed(&db);
    let a = start(db.clone(), &adir);
    a.export_now().unwrap();
    type Memories<'t> = redb::Table<'t, [u8; 16], MemoryMetadata>;
    let edit = |f: &dyn Fn(&redb::WriteTransaction, &mut Memories<'_>)| {
        let wtxn = db.write_txn().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            f(&wtxn, &mut mem);
        }
        wtxn.commit().unwrap();
    };
    let check = || {
        let rows: Vec<MemoryMetadata> = {
            let rtxn = db.read_txn().unwrap();
            let t = rtxn.open_table(MEMORIES_TABLE).unwrap();
            t.iter().unwrap().map(|e| e.unwrap().1.value()).collect()
        };
        let q = ScanQuery {
            namespace_id: 1,
            space_id: space(1),
            statuses: vec![0, 1, 2],
            group_by_kind: true,
            at_unix_nanos: T0 + 40 * DAY,
            ..ScanQuery::default()
        };
        let got = futures_lite::future::block_on(a.query(q.clone())).unwrap();
        assert_same(&got.groups, &brute(&rows, &q), &q);
    };
    edit(&|w, mem| {
        for slot in 0..20u64 {
            let id = MemoryId::pack(0, slot, 1).to_be_bytes();
            let mut r = mem.get(&id).unwrap().unwrap().value();
            r.access_count += 7;
            put_memory(w, mem, id, &r).unwrap();
        }
    });
    assert_eq!(a.refresh_now().unwrap().rows, 20);
    check();
    edit(&|w, mem| {
        for slot in 100..110u64 {
            remove_memory(w, mem, MemoryId::pack(0, slot, 1).to_be_bytes()).unwrap();
        }
    });
    // Cumulative since the base: the 20 updates and the 10 deletions.
    assert_eq!(a.refresh_now().unwrap().rows, 30);
    check();
}
