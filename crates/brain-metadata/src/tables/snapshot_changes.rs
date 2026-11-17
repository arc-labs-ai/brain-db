//! Change log for incremental analytics snapshots.
//!
//! While analytics is enabled, every committed change to a memory's
//! exported fields appends `(change_seq, memory_id)` here, in the same write
//! txn (from `agg::AggDeltas::flush`, so the counters' one-rule guard
//! covers it too). `change_seq` goes up by one per write txn.
//!
//! A full snapshot records the `change_seq` its read txn saw; a later delta
//! export reads only the ids logged after it. Once a newer full snapshot
//! is in use, the shard prunes everything up to its `change_seq`.
//!
//! The log only exists while tracking is on: [`set_tracking`] turns it on
//! at shard start when analytics is enabled (and reports that any older
//! snapshot can't be trusted), and off — dropping the log — when not, so a
//! shard without analytics pays nothing.

use std::collections::BTreeMap;

use redb::{ReadTransaction, ReadableTable, TableDefinition, WriteTransaction};

use super::agg::AGG_STATE_TABLE;
use super::memory::MemoryMetadata;

/// `(change_seq, memory_id) → snapshot bucket`. The bucket is kept so a
/// deleted memory (no row left to read it from) still reaches its bucket.
pub const SNAPSHOT_CHANGES_TABLE: TableDefinition<'static, (u64, [u8; 16]), u8> =
    TableDefinition::new("snapshot_changes");

/// Snapshots split memories into this many files by `(namespace, space)`,
/// so a query for one space opens one file.
pub const SNAPSHOT_BUCKETS: u8 = 16;

/// The snapshot bucket of a `(namespace, space)` (FNV-1a; stable across
/// releases — snapshots on disk depend on it).
#[must_use]
pub fn bucket_of(namespace_id: u32, space_id: &[u8; 16]) -> u8 {
    let mut h: u32 = 0x811c_9dc5;
    for b in namespace_id.to_be_bytes().iter().chain(space_id) {
        h = (h ^ u32::from(*b)).wrapping_mul(0x0100_0193);
    }
    (h % u32::from(SNAPSHOT_BUCKETS)) as u8
}

/// [`AGG_STATE_TABLE`] key present while tracking is on.
pub const TRACKING: &str = "snapshot_tracking";
/// [`AGG_STATE_TABLE`] key holding the last `change_seq` (first 8 bytes BE).
const SEQ: &str = "snapshot_change_seq";

fn u64_of(b: [u8; 16]) -> u64 {
    u64::from_be_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn bytes_of(v: u64) -> [u8; 16] {
    let mut b = [0u8; 16];
    b[..8].copy_from_slice(&v.to_be_bytes());
    b
}

/// Whether two versions of a row export identically (so an update that
/// only touches, say, edge counts is not logged).
#[must_use]
pub fn exported_fields_eq(a: &MemoryMetadata, b: &MemoryMetadata) -> bool {
    a.namespace_id == b.namespace_id
        && a.space_id_bytes == b.space_id_bytes
        && a.kind == b.kind
        && a.flags == b.flags
        && a.salience_anchor == b.salience_anchor
        && a.salience_anchor_at_unix_nanos == b.salience_anchor_at_unix_nanos
        && a.salience_initial == b.salience_initial
        && a.access_count == b.access_count
        && a.created_at_unix_nanos == b.created_at_unix_nanos
        && a.session_id == b.session_id
}

/// Append `(id, bucket)`s under a new `change_seq`. Called only while
/// tracking.
pub(super) fn log(
    wtxn: &WriteTransaction,
    ids: impl Iterator<Item = ([u8; 16], u8)>,
) -> Result<(), redb::Error> {
    let seq = {
        let mut state = wtxn.open_table(AGG_STATE_TABLE)?;
        let next = state.get(SEQ)?.map_or(0, |g| u64_of(g.value())) + 1;
        state.insert(SEQ, bytes_of(next))?;
        next
    };
    let mut t = wtxn.open_table(SNAPSHOT_CHANGES_TABLE)?;
    for (id, bucket) in ids.collect::<BTreeMap<_, _>>() {
        t.insert((seq, id), bucket)?;
    }
    Ok(())
}

/// Turn change tracking on or off. Returns `true` when it was off before
/// (or is now off): snapshots taken without tracking can't be extended
/// with deltas, so the caller must start from a full export.
pub fn set_tracking(wtxn: &WriteTransaction, on: bool) -> Result<bool, redb::Error> {
    let mut state = wtxn.open_table(AGG_STATE_TABLE)?;
    let was_on = state.get(TRACKING)?.is_some();
    if on {
        if !was_on {
            state.insert(TRACKING, [0u8; 16])?;
        }
    } else if was_on {
        state.remove(TRACKING)?;
        drop(state);
        wtxn.delete_table(SNAPSHOT_CHANGES_TABLE)?;
        wtxn.open_table(SNAPSHOT_CHANGES_TABLE)?;
    }
    Ok(!(on && was_on))
}

/// The last `change_seq` committed as of `rtxn` (0 = none yet).
pub fn current_seq(rtxn: &ReadTransaction) -> Result<u64, redb::Error> {
    let state = rtxn.open_table(AGG_STATE_TABLE)?;
    let v = state.get(SEQ)?.map_or(0, |g| u64_of(g.value()));
    Ok(v)
}

/// Distinct ids changed after `since` as of `rtxn`, with their snapshot
/// bucket, or `None` once there are more than `max`.
pub fn changed_since(
    rtxn: &ReadTransaction,
    since: u64,
    max: usize,
) -> Result<Option<BTreeMap<[u8; 16], u8>>, redb::Error> {
    let t = rtxn.open_table(SNAPSHOT_CHANGES_TABLE)?;
    let mut ids = BTreeMap::new();
    for e in t.range((since.saturating_add(1), [0u8; 16])..)? {
        let (k, v) = e?;
        ids.insert(k.value().1, v.value());
        if ids.len() > max {
            return Ok(None);
        }
    }
    Ok(Some(ids))
}

/// Delete up to `max` log entries with `change_seq <= upto`, oldest first.
/// Returns how many were deleted.
pub fn prune(wtxn: &WriteTransaction, upto: u64, max: usize) -> Result<usize, redb::Error> {
    let mut t = wtxn.open_table(SNAPSHOT_CHANGES_TABLE)?;
    let keys: Vec<(u64, [u8; 16])> = t
        .range(..=(upto, [0xff; 16]))?
        .take(max)
        .map(|e| e.map(|(k, _)| k.value()))
        .collect::<Result<_, _>>()?;
    for k in &keys {
        t.remove(*k)?;
    }
    Ok(keys.len())
}

/// Whether any log entry at or below `upto` remains (read-only check, so
/// an idle prune commits nothing).
pub fn prune_due(rtxn: &ReadTransaction, upto: u64) -> Result<bool, redb::Error> {
    let t = rtxn.open_table(SNAPSHOT_CHANGES_TABLE)?;
    let due = t.range(..=(upto, [0xff; 16]))?.next().is_some();
    Ok(due)
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::agg::{put_memory, remove_memory};
    use crate::tables::fresh_db;
    use crate::tables::memory::MEMORIES_TABLE;
    use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
    use redb::ReadableDatabase;

    fn row(slot: u64) -> MemoryMetadata {
        MemoryMetadata::new_active(
            MemoryId::pack(0, slot, 1),
            NamespaceId::from(1),
            SpaceId::default(),
            SessionId(1),
            slot,
            1,
            MemoryKind::Episodic,
            [0; 16],
            0.5,
            1,
            1_800_000_000_000_000_000,
        )
    }

    #[test]
    fn logs_changes_only_while_tracking_and_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let write = |f: &dyn Fn(&WriteTransaction)| {
            let wtxn = db.begin_write().unwrap();
            f(&wtxn);
            wtxn.commit().unwrap();
        };
        let put = |slot: u64, edit: &dyn Fn(&mut MemoryMetadata)| {
            write(&|w| {
                let mut t = w.open_table(MEMORIES_TABLE).unwrap();
                let mut r = t
                    .get(MemoryId::pack(0, slot, 1).to_be_bytes())
                    .unwrap()
                    .map_or_else(|| row(slot), |g| g.value());
                edit(&mut r);
                put_memory(w, &mut t, r.memory_id_bytes, &r).unwrap();
            });
        };

        put(1, &|_| {}); // not tracking yet: not logged
        write(&|w| assert!(set_tracking(w, true).unwrap(), "was off"));
        write(&|w| assert!(!set_tracking(w, true).unwrap(), "already on"));
        let base = current_seq(&db.begin_read().unwrap()).unwrap();

        put(2, &|_| {});
        put(1, &|r| r.edges_out_count += 1); // not an exported field
        put(1, &|r| r.access_count += 1);
        put(3, &|_| {});
        write(&|w| {
            let mut t = w.open_table(MEMORIES_TABLE).unwrap();
            remove_memory(w, &mut t, MemoryId::pack(0, 3, 1).to_be_bytes()).unwrap();
        });
        let rtxn = db.begin_read().unwrap();
        let ids = changed_since(&rtxn, base, 100).unwrap().unwrap();
        let bucket = bucket_of(1, &[0u8; 16]);
        let want: BTreeMap<_, _> = [1u64, 2, 3]
            .iter()
            .map(|s| (MemoryId::pack(0, *s, 1).to_be_bytes(), bucket))
            .collect();
        assert_eq!(ids, want, "ids with their bucket, deleted one included");
        assert!(changed_since(&rtxn, base, 2).unwrap().is_none(), "over max");
        let mid = current_seq(&rtxn).unwrap() - 1;
        drop(rtxn);

        write(&|w| assert_eq!(prune(w, mid, 2).unwrap(), 2));
        assert!(prune_due(&db.begin_read().unwrap(), mid).unwrap());
        write(&|w| assert_eq!(prune(w, mid, 100).unwrap(), 1));
        assert!(!prune_due(&db.begin_read().unwrap(), mid).unwrap());
        let rtxn = db.begin_read().unwrap();
        assert_eq!(
            changed_since(&rtxn, 0, 100).unwrap().unwrap().len(),
            1,
            "last txn kept"
        );
        drop(rtxn);

        write(&|w| assert!(set_tracking(w, false).unwrap()));
        put(4, &|_| {});
        let rtxn = db.begin_read().unwrap();
        assert!(
            changed_since(&rtxn, 0, 100).unwrap().unwrap().is_empty(),
            "dropped, off"
        );
    }

    #[test]
    fn buckets_spread_spaces_and_stay_stable() {
        let mut seen = [0u32; SNAPSHOT_BUCKETS as usize];
        for ns in 0..8u32 {
            for sp in 0..64u8 {
                let mut space = [0u8; 16];
                space[15] = sp;
                seen[usize::from(bucket_of(ns, &space))] += 1;
            }
        }
        assert!(seen.iter().all(|n| *n > 8), "every bucket used: {seen:?}");
        // Pinned: snapshots on disk depend on it.
        assert_eq!(bucket_of(7, &[3u8; 16]), 0);
    }
}
