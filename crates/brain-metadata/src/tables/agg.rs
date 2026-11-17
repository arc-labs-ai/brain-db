//! `agg_counters`: running totals over `memories`, so COUNT / SUM / AVG
//! filtered by namespace, space, kind and status answer without a scan.
//!
//! One row per `(namespace_id, space_id, kind, status)` holds the number
//! of memories in that bucket, the sum of their access counts, and their
//! salience sum in a form that stays exact while salience decays (see
//! below).
//!
//! ## The one rule
//!
//! Every write to `MEMORIES_TABLE` must go through [`put_memory`] /
//! [`remove_memory`] (or their `_deferred` batch forms). Each one
//! subtracts the old row's contribution and adds the new row's, in the
//! same write txn, so the counters equal a full scan of `memories` at
//! every commit. `tests/agg_write_guard.rs` fails the build if a non-test
//! file opens `MEMORIES_TABLE` for writing anywhere else.
//!
//! MIN / MAX are deliberately absent: a deleted minimum can't be
//! subtracted back out of a running total.
//!
//! ## Decaying salience (forward decay)
//!
//! Salience decays when read (`crate::salience`): a row's value at `now`
//! is `floor + (anchor − floor) × 2^(−(now − anchor_at)/h)`. That is a
//! constant plus a multiple of one per-kind factor, so a bucket keeps the
//! exact `floor_sum_micro` and the sum of forward-decay weights
//! `(anchor − floor) × 2^((anchor_at − L)/h)`; `weight_sum × 2^(−(now − L)/h)`
//! is then the exact sum of the excesses at any `now`. The weights never
//! change as time passes, so no write is needed while rows decay. The
//! weight sum is compensated (Neumaier), so add/subtract cycles don't
//! accumulate rounding error.
//!
//! ## Online build
//!
//! A DB written before the counters existed gets them without a startup
//! scan: [`backfill_if_missing`] only plants a build cursor, and
//! [`build_step`] (driven by the `agg_counters` worker) counts memories in
//! id order, a small batch per write txn. Rows below the cursor are
//! counted, rows at or above it are not yet — so writes apply deltas only
//! below the cursor and the build picks up the rest. [`query`] answers
//! `None` until the cursor is gone.
//!
//! ## Drift repair
//!
//! [`verify`] recomputes the counted rows from one snapshot and compares;
//! [`apply_corrections`] adds `actual - stored` later. That stays correct
//! even after more writes, because every write moves the counters by its
//! own delta.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use redb::{
    ReadTransaction, ReadableTable, StorageError, Table, TableDefinition, WriteTransaction,
};

use super::memory::{flags, MemoryMetadata, MEMORIES_TABLE};
use crate::salience::forward_scale;

/// `(namespace_id, space_id_bytes, kind, status)`.
pub type AggKey = (u32, [u8; 16], u8, u8);
/// `(count, floor_sum_micro, access_sum, weight_sum, weight_comp)`.
type AggValue = (i64, i64, i64, f64, f64);

/// The counters table. Small: one row per live bucket, independent of
/// how many memories exist.
pub const AGG_COUNTERS_TABLE: TableDefinition<'static, AggKey, AggValue> =
    TableDefinition::new("agg_counters");

/// Counter state: [`BUILD_CURSOR`] while an online build is in progress
/// (memory ids `< cursor` are counted), and the snapshot change-log keys.
pub const AGG_STATE_TABLE: TableDefinition<'static, &str, [u8; 16]> =
    TableDefinition::new("agg_state");

/// Key of the build cursor in [`AGG_STATE_TABLE`].
pub const BUILD_CURSOR: &str = "build_cursor";

/// Bucket status, derived from [`MemoryMetadata::flags`].
pub mod status {
    pub const ACTIVE: u8 = 0;
    pub const TOMBSTONED: u8 = 1;
    pub const HARD_FORGOTTEN: u8 = 2;
}

/// Floor values are stored as `round(value * 1e6)` so their sums are
/// exact integers.
pub const SALIENCE_SCALE: f64 = 1_000_000.0;

/// Raw totals for one bucket, as stored.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Agg {
    pub count: i64,
    /// Sum of the rows' floors, micro-units (exact).
    pub floor_sum_micro: i64,
    pub access_sum: i64,
    /// Sum of forward-decay weights (see the module docs) …
    pub weight_sum: f64,
    /// … and its Neumaier compensation term: the sum is
    /// `weight_sum + weight_comp`.
    pub weight_comp: f64,
}

/// `sum += x`, Neumaier-compensated into `comp`.
fn neumaier(sum: &mut f64, comp: &mut f64, x: f64) {
    let t = *sum + x;
    *comp += if sum.abs() >= x.abs() {
        (*sum - t) + x
    } else {
        (x - t) + *sum
    };
    *sum = t;
}

impl Agg {
    fn add(&mut self, o: Agg, sign: i64) {
        self.count += sign * o.count;
        self.floor_sum_micro += sign * o.floor_sum_micro;
        self.access_sum += sign * o.access_sum;
        let s = sign as f64;
        neumaier(
            &mut self.weight_sum,
            &mut self.weight_comp,
            s * o.weight_sum,
        );
        neumaier(
            &mut self.weight_sum,
            &mut self.weight_comp,
            s * o.weight_comp,
        );
    }

    /// The compensated weight sum.
    #[must_use]
    pub fn weight(self) -> f64 {
        self.weight_sum + self.weight_comp
    }

    fn is_noop(self) -> bool {
        self.count == 0 && self.floor_sum_micro == 0 && self.access_sum == 0 && self.weight() == 0.0
    }

    /// Salience sum of the bucket's rows at `now`.
    fn salience_sum_at(self, kind: u8, now: u64) -> f64 {
        self.floor_sum_micro as f64 / SALIENCE_SCALE + self.weight() * forward_scale(kind, now)
    }

    /// Equal up to float rounding in the weight sum: within a millionth
    /// of a salience unit per row, measured at `now`.
    fn matches(self, other: Agg, kind: u8, now: u64) -> bool {
        self.count == other.count
            && self.floor_sum_micro == other.floor_sum_micro
            && self.access_sum == other.access_sum
            && (self.weight() - other.weight()).abs() * forward_scale(kind, now)
                <= 1e-6 * (self.count.max(other.count).max(1) as f64)
    }

    fn from_value((count, floor_sum_micro, access_sum, weight_sum, weight_comp): AggValue) -> Self {
        Self {
            count,
            floor_sum_micro,
            access_sum,
            weight_sum,
            weight_comp,
        }
    }

    fn to_value(self) -> AggValue {
        (
            self.count,
            self.floor_sum_micro,
            self.access_sum,
            self.weight_sum,
            self.weight_comp,
        )
    }
}

/// Totals for one query group, evaluated at a point in time.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct AggTotal {
    pub count: i64,
    pub salience_sum: f64,
    pub access_sum: i64,
}

impl AggTotal {
    /// Mean salience; `None` for an empty group.
    #[must_use]
    pub fn avg_salience(self) -> Option<f64> {
        (self.count > 0).then(|| self.salience_sum / self.count as f64)
    }
}

/// The bucket status a row counts under.
#[must_use]
pub fn status_of(m: &MemoryMetadata) -> u8 {
    if m.flags & flags::HARD_FORGOTTEN != 0 {
        status::HARD_FORGOTTEN
    } else if m.flags & flags::ACTIVE == 0 {
        status::TOMBSTONED
    } else {
        status::ACTIVE
    }
}

/// `round(salience * 1e6)`; the exact fixed-point form floor sums use.
#[must_use]
pub fn salience_micro(s: f32) -> i64 {
    if s.is_finite() {
        (f64::from(s) * SALIENCE_SCALE).round() as i64
    } else {
        0
    }
}

/// A row's time-independent counter inputs.
#[derive(Debug, Clone, Copy)]
struct RowContrib {
    key: AggKey,
    agg: Agg,
}

impl RowContrib {
    fn of(m: &MemoryMetadata) -> Self {
        Self {
            key: (m.namespace_id, m.space_id_bytes, m.kind, status_of(m)),
            agg: Agg {
                count: 1,
                floor_sum_micro: salience_micro(m.salience_floor()),
                access_sum: i64::from(m.access_count),
                weight_sum: m.salience_forward_weight(),
                weight_comp: 0.0,
            },
        }
    }
}

/// Counter changes accumulated in memory and written once per txn.
/// Batch writers (access boost, recovery salience batches, ...) use this
/// so N row updates cost one counter write per touched bucket.
#[derive(Debug, Default)]
pub struct AggDeltas {
    rows: Vec<([u8; 16], i64, RowContrib)>,
    /// Ids (with snapshot bucket) whose exported fields changed, for the
    /// snapshot change log.
    changed: Vec<([u8; 16], u8)>,
}

impl AggDeltas {
    /// Record a change to memory `id`: `old` → `new` (`None` = absent).
    pub fn record(
        &mut self,
        id: [u8; 16],
        old: Option<&MemoryMetadata>,
        new: Option<&MemoryMetadata>,
    ) {
        if let Some(o) = old {
            self.rows.push((id, -1, RowContrib::of(o)));
        }
        if let Some(n) = new {
            self.rows.push((id, 1, RowContrib::of(n)));
        }
        let same = match (old, new) {
            (Some(o), Some(n)) => super::snapshot_changes::exported_fields_eq(o, n),
            (None, None) => true,
            _ => false,
        };
        if !same {
            if let Some(m) = new.or(old) {
                let bucket = super::snapshot_changes::bucket_of(m.namespace_id, &m.space_id_bytes);
                self.changed.push((id, bucket));
            }
        }
    }

    /// Apply and clear the pending changes. Skips rows an online build has
    /// not reached yet (it will count their final state). Does not touch
    /// the counters table when nothing changed (e.g. an edge-count-only
    /// row update).
    pub fn flush(&mut self, wtxn: &WriteTransaction) -> Result<(), redb::Error> {
        if self.rows.is_empty() && self.changed.is_empty() {
            return Ok(());
        }
        let (cursor, tracking) = {
            let state = wtxn.open_table(AGG_STATE_TABLE)?;
            let tracking = state.get(super::snapshot_changes::TRACKING)?.is_some();
            (read_cursor(&state)?, tracking)
        };
        if tracking && !self.changed.is_empty() {
            super::snapshot_changes::log(wtxn, self.changed.drain(..))?;
        }
        self.changed.clear();
        let mut by_key: BTreeMap<AggKey, Agg> = BTreeMap::new();
        for (id, sign, rc) in self.rows.drain(..) {
            if cursor.is_some_and(|c| id >= c) {
                continue;
            }
            by_key.entry(rc.key).or_default().add(rc.agg, sign);
        }
        by_key.retain(|_, a| !a.is_noop());
        if by_key.is_empty() {
            return Ok(());
        }
        add_to_counters(wtxn, by_key)
    }
}

fn add_to_counters(
    wtxn: &WriteTransaction,
    deltas: BTreeMap<AggKey, Agg>,
) -> Result<(), redb::Error> {
    let mut t = wtxn.open_table(AGG_COUNTERS_TABLE)?;
    for (k, delta) in deltas {
        let mut cur = t
            .get(&k)?
            .map_or_else(Agg::default, |g| Agg::from_value(g.value()));
        cur.add(delta, 1);
        // An empty bucket's other fields are zero up to weight rounding.
        if cur.count == 0 {
            t.remove(&k)?;
        } else {
            t.insert(&k, cur.to_value())?;
        }
    }
    Ok(())
}

fn read_cursor(
    state: &impl ReadableTable<&'static str, [u8; 16]>,
) -> Result<Option<[u8; 16]>, StorageError> {
    Ok(state.get(BUILD_CURSOR)?.map(|g| g.value()))
}

/// The online-build cursor, if a build is in progress.
pub fn build_cursor(rtxn: &ReadTransaction) -> Result<Option<[u8; 16]>, redb::Error> {
    Ok(read_cursor(&rtxn.open_table(AGG_STATE_TABLE)?)?)
}

type MemoriesTable<'txn> = Table<'txn, [u8; 16], MemoryMetadata>;

// One batch per thread: redb allows a single write txn per database, and
// each shard (one database) runs on its own thread, so while a batch is
// open no other txn on this thread can write through `put_memory`. The
// txn's address is kept anyway, so a write on a different txn never lands
// in another's batch.
thread_local! {
    static BATCH: std::cell::RefCell<Option<(usize, AggDeltas)>> =
        const { std::cell::RefCell::new(None) };
}

fn txn_addr(wtxn: &WriteTransaction) -> usize {
    std::ptr::from_ref(wtxn) as usize
}

/// Collects the counter changes of every [`put_memory`] / [`remove_memory`]
/// on one write txn and applies them once, in [`Self::finish`] — one
/// counter write per touched bucket per txn instead of per row. Dropping
/// it unfinished (an error path that abandons the txn) discards them.
#[must_use = "call finish() before committing"]
pub struct TxnBatch {
    addr: usize,
    owner: bool,
}

/// Start batching counter changes for `wtxn` (see [`TxnBatch`]). Nested
/// calls for the same txn join the outer batch.
pub fn begin_batch(wtxn: &WriteTransaction) -> TxnBatch {
    let addr = txn_addr(wtxn);
    let owner = BATCH.with_borrow_mut(|b| match b {
        Some((a, _)) if *a == addr => false,
        _ => {
            *b = Some((addr, AggDeltas::default()));
            true
        }
    });
    TxnBatch { addr, owner }
}

impl TxnBatch {
    /// Apply the collected changes inside `wtxn` (the txn this batch was
    /// started for). Call before committing.
    pub fn finish(mut self, wtxn: &WriteTransaction) -> Result<(), redb::Error> {
        debug_assert_eq!(self.addr, txn_addr(wtxn), "finish on the batch's own txn");
        if !self.owner {
            return Ok(());
        }
        self.owner = false;
        let deltas = BATCH.with_borrow_mut(|b| b.take().map(|(_, d)| d));
        match deltas {
            Some(mut d) => d.flush(wtxn),
            None => Ok(()),
        }
    }
}

impl Drop for TxnBatch {
    fn drop(&mut self) {
        if self.owner {
            BATCH.with_borrow_mut(|b| {
                if b.as_ref().is_some_and(|(a, _)| *a == self.addr) {
                    *b = None;
                }
            });
        }
    }
}

/// Run `f` with the open batch for `wtxn`, if any.
fn with_batch<T>(wtxn: &WriteTransaction, f: impl FnOnce(&mut AggDeltas) -> T) -> Option<T> {
    let addr = txn_addr(wtxn);
    BATCH.with_borrow_mut(|b| match b {
        Some((a, d)) if *a == addr => Some(f(d)),
        _ => None,
    })
}

/// Insert or overwrite a memory row and update the counters in the same
/// txn — at once, or at [`TxnBatch::finish`] when a batch is open for
/// `wtxn`. Returns the previous row. `memories` must be `MEMORIES_TABLE`
/// opened on `wtxn`.
pub fn put_memory(
    wtxn: &WriteTransaction,
    memories: &mut MemoriesTable<'_>,
    id: [u8; 16],
    row: &MemoryMetadata,
) -> Result<Option<MemoryMetadata>, redb::Error> {
    let old = memories.insert(&id, row)?.map(|g| g.value());
    let batched = with_batch(wtxn, |d| d.record(id, old.as_ref(), Some(row)));
    if batched.is_none() {
        let mut d = AggDeltas::default();
        d.record(id, old.as_ref(), Some(row));
        d.flush(wtxn)?;
    }
    Ok(old)
}

/// Delete a memory row and update the counters in the same txn (at once,
/// or at [`TxnBatch::finish`]). Returns the removed row, if any.
pub fn remove_memory(
    wtxn: &WriteTransaction,
    memories: &mut MemoriesTable<'_>,
    id: [u8; 16],
) -> Result<Option<MemoryMetadata>, redb::Error> {
    let old = memories.remove(&id)?.map(|g| g.value());
    let batched = with_batch(wtxn, |d| d.record(id, old.as_ref(), None));
    if batched.is_none() {
        let mut d = AggDeltas::default();
        d.record(id, old.as_ref(), None);
        d.flush(wtxn)?;
    }
    Ok(old)
}

/// [`put_memory`] for batches: records the counter change in `deltas`;
/// the caller must [`AggDeltas::flush`] before committing.
pub fn put_memory_deferred(
    memories: &mut MemoriesTable<'_>,
    id: [u8; 16],
    row: &MemoryMetadata,
    deltas: &mut AggDeltas,
) -> Result<Option<MemoryMetadata>, StorageError> {
    let old = memories.insert(&id, row)?.map(|g| g.value());
    deltas.record(id, old.as_ref(), Some(row));
    Ok(old)
}

/// [`remove_memory`] for batches.
pub fn remove_memory_deferred(
    memories: &mut MemoriesTable<'_>,
    id: [u8; 16],
    deltas: &mut AggDeltas,
) -> Result<Option<MemoryMetadata>, StorageError> {
    let old = memories.remove(&id)?.map(|g| g.value());
    deltas.record(id, old.as_ref(), None);
    Ok(old)
}

// ---------------------------------------------------------------------------
// Rebuild / verify.
// ---------------------------------------------------------------------------

/// Recount the memories an online build has counted (`id < cursor`; all
/// of them when `cursor` is `None`). Checks `stop` every 64K rows and
/// returns `None` once it says so — a long scan off the shard thread must
/// end promptly when the shard stops.
fn recount_until(
    memories: &impl ReadableTable<[u8; 16], MemoryMetadata>,
    cursor: Option<[u8; 16]>,
    stop: &dyn Fn() -> bool,
) -> Result<Option<BTreeMap<AggKey, Agg>>, StorageError> {
    let mut out: BTreeMap<AggKey, Agg> = BTreeMap::new();
    let iter = match cursor {
        Some(c) => memories.range(..c)?,
        None => memories.range::<[u8; 16]>(..)?,
    };
    for (i, entry) in iter.enumerate() {
        if i % 65_536 == 0 && stop() {
            return Ok(None);
        }
        let (_, v) = entry?;
        let rc = RowContrib::of(&v.value());
        out.entry(rc.key).or_default().add(rc.agg, 1);
    }
    Ok(Some(out))
}

/// Start an online build if the counters are missing (first open of a DB
/// written before they existed): plants the build cursor at the lowest id
/// and returns `true`. No scan here, so it does not slow the open; the
/// `agg_counters` worker does the counting. Called from
/// `open_or_init_schema` next to the other derived-index backfills.
///
/// Every memory row contributes to some bucket, so "no counters, no build
/// in progress, but some memories" can only mean "never built".
pub fn backfill_if_missing(wtxn: &WriteTransaction) -> Result<bool, redb::Error> {
    let needs_build = {
        let memories = wtxn.open_table(MEMORIES_TABLE)?;
        let counters = wtxn.open_table(AGG_COUNTERS_TABLE)?;
        let state = wtxn.open_table(AGG_STATE_TABLE)?;
        counters.first()?.is_none() && memories.first()?.is_some() && read_cursor(&state)?.is_none()
    };
    if needs_build {
        wtxn.open_table(AGG_STATE_TABLE)?
            .insert(BUILD_CURSOR, [0u8; 16])?;
    }
    Ok(needs_build)
}

/// Outcome of one [`build_step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuildStep {
    /// No build in progress.
    Idle,
    /// Counted `rows` more; more remain.
    Progress { rows: usize },
    /// Counted the last `rows`; counters are complete.
    Finished { rows: usize },
}

/// Count up to `max_rows` more memories for an online build, inside
/// `wtxn`. Each step is short so the shard writer is never blocked long.
pub fn build_step(wtxn: &WriteTransaction, max_rows: usize) -> Result<BuildStep, redb::Error> {
    let Some(cursor) = read_cursor(&wtxn.open_table(AGG_STATE_TABLE)?)? else {
        return Ok(BuildStep::Idle);
    };
    let mut adds: BTreeMap<AggKey, Agg> = BTreeMap::new();
    let mut rows = 0usize;
    let mut next: Option<[u8; 16]> = None;
    {
        let memories = wtxn.open_table(MEMORIES_TABLE)?;
        for entry in memories.range(cursor..)? {
            let (k, v) = entry?;
            if rows == max_rows {
                next = Some(k.value());
                break;
            }
            let rc = RowContrib::of(&v.value());
            adds.entry(rc.key).or_default().add(rc.agg, 1);
            rows += 1;
        }
    }
    add_to_counters(wtxn, adds)?;
    let mut state = wtxn.open_table(AGG_STATE_TABLE)?;
    Ok(match next {
        Some(n) => {
            state.insert(BUILD_CURSOR, n)?;
            BuildStep::Progress { rows }
        }
        None => {
            state.remove(BUILD_CURSOR)?;
            BuildStep::Finished { rows }
        }
    })
}

/// One bucket where stored counters disagree with a recompute.
#[derive(Debug, Clone, PartialEq)]
pub struct AggDrift {
    pub key: AggKey,
    pub stored: Agg,
    pub actual: Agg,
}

/// Result of [`verify`]: buckets to fix with [`apply_corrections`].
#[derive(Debug, Default, Clone, PartialEq)]
pub struct VerifyReport {
    pub drift: Vec<AggDrift>,
}

impl VerifyReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.drift.is_empty()
    }
}

/// Compare stored counters to a recount of the counted rows, both from
/// the same snapshot; weight sums are compared at `now`. A non-clean
/// result means some write bypassed [`put_memory`].
pub fn verify(rtxn: &ReadTransaction, now: u64) -> Result<VerifyReport, redb::Error> {
    Ok(verify_until(rtxn, now, &|| false)?.unwrap_or_default())
}

/// [`verify`] that gives up (returns `None`) once `stop` says so.
pub fn verify_until(
    rtxn: &ReadTransaction,
    now: u64,
    stop: &dyn Fn() -> bool,
) -> Result<Option<VerifyReport>, redb::Error> {
    let cursor = read_cursor(&rtxn.open_table(AGG_STATE_TABLE)?)?;
    let Some(actual) = recount_until(&rtxn.open_table(MEMORIES_TABLE)?, cursor, stop)? else {
        return Ok(None);
    };
    let mut stored: BTreeMap<AggKey, Agg> = BTreeMap::new();
    for entry in rtxn.open_table(AGG_COUNTERS_TABLE)?.iter()? {
        let (k, v) = entry?;
        stored.insert(k.value(), Agg::from_value(v.value()));
    }
    let mut keys: Vec<AggKey> = stored.keys().chain(actual.keys()).copied().collect();
    keys.sort_unstable();
    keys.dedup();
    let drift = keys
        .into_iter()
        .filter_map(|key| {
            let s = stored.get(&key).copied().unwrap_or_default();
            let a = actual.get(&key).copied().unwrap_or_default();
            (!s.matches(a, key.2, now)).then_some(AggDrift {
                key,
                stored: s,
                actual: a,
            })
        })
        .collect();
    Ok(Some(VerifyReport { drift }))
}

/// Repair drift found by [`verify`] on an earlier snapshot: adds
/// `actual - stored` per bucket. Valid after intervening writes, which
/// each moved the counters by their own delta.
pub fn apply_corrections(wtxn: &WriteTransaction, drift: &[AggDrift]) -> Result<(), redb::Error> {
    let mut deltas: BTreeMap<AggKey, Agg> = BTreeMap::new();
    for d in drift {
        let mut a = d.actual;
        a.add(d.stored, -1);
        deltas.insert(d.key, a);
    }
    add_to_counters(wtxn, deltas)
}

// ---------------------------------------------------------------------------
// Maintenance state (driven by `brain-workers`' AggCountersWorker, on the
// shard executor so every counter write is serialized with the shard's other
// writes — including the metadata file copy a snapshot takes).
// ---------------------------------------------------------------------------

/// What counter maintenance has done; read by `/metrics`.
#[derive(Debug, Default)]
pub struct MaintenanceStats {
    /// `false` while an online build is in progress.
    pub counters_ready: AtomicBool,
    pub build_rows: AtomicU64,
    pub verify_runs: AtomicU64,
    /// Buckets found drifted (and repaired) across all checks. Should
    /// stay 0; anything else means a write path bypassed the counters.
    pub drifted_buckets: AtomicU64,
}

impl MaintenanceStats {
    /// Record a finished build step.
    pub fn record_build(&self, step: BuildStep) {
        let (rows, done) = match step {
            BuildStep::Idle => (0, true),
            BuildStep::Progress { rows } => (rows, false),
            BuildStep::Finished { rows } => (rows, true),
        };
        self.build_rows.fetch_add(rows as u64, Ordering::Relaxed);
        self.counters_ready.store(done, Ordering::Relaxed);
    }

    /// Record a finished verify.
    pub fn record_verify(&self, report: &VerifyReport) {
        self.verify_runs.fetch_add(1, Ordering::Relaxed);
        self.drifted_buckets
            .fetch_add(report.drift.len() as u64, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Query.
// ---------------------------------------------------------------------------

/// Which buckets to include. `None` / empty = no restriction, except
/// `statuses`, where empty means active memories only.
#[derive(Debug, Clone, Default)]
pub struct AggFilter {
    pub namespace_id: Option<u32>,
    pub space_id: Option<[u8; 16]>,
    pub kinds: Vec<u8>,
    pub statuses: Vec<u8>,
}

impl AggFilter {
    fn admits(&self, (ns, sp, kind, st): AggKey) -> bool {
        let status_ok = if self.statuses.is_empty() {
            st == status::ACTIVE
        } else {
            self.statuses.contains(&st)
        };
        status_ok
            && self.namespace_id.is_none_or(|n| n == ns)
            && self.space_id.is_none_or(|s| s == sp)
            && (self.kinds.is_empty() || self.kinds.contains(&kind))
    }
}

/// Dimensions to group by; all `false` = one grand total.
#[derive(Debug, Clone, Copy, Default)]
pub struct AggGroupBy {
    pub namespace: bool,
    pub space: bool,
    pub kind: bool,
    pub status: bool,
}

impl AggGroupBy {
    fn group(self, (ns, sp, kind, st): AggKey) -> AggGroup {
        AggGroup {
            namespace_id: self.namespace.then_some(ns),
            space_id: self.space.then_some(sp),
            kind: self.kind.then_some(kind),
            status: self.status.then_some(st),
        }
    }
}

/// One result group. Fields not grouped on are `None`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AggGroup {
    pub namespace_id: Option<u32>,
    pub space_id: Option<[u8; 16]>,
    pub kind: Option<u8>,
    pub status: Option<u8>,
}

/// Sum the matching buckets into groups, with salience evaluated at
/// `now`. Reads only counter rows: a namespace filter is a range read,
/// anything else a scan of the small counters table. `None` while an
/// online build is still counting.
pub fn query(
    rtxn: &ReadTransaction,
    filter: &AggFilter,
    group_by: AggGroupBy,
    now: u64,
) -> Result<Option<BTreeMap<AggGroup, AggTotal>>, redb::Error> {
    let cursor = read_cursor(&rtxn.open_table(AGG_STATE_TABLE)?)?;
    if cursor.is_some() {
        return Ok(None);
    }
    let t = rtxn.open_table(AGG_COUNTERS_TABLE)?;
    let range = match (filter.namespace_id, filter.space_id) {
        (Some(ns), Some(sp)) => t.range((ns, sp, 0, 0)..=(ns, sp, u8::MAX, u8::MAX))?,
        (Some(ns), None) => t.range((ns, [0; 16], 0, 0)..=(ns, [0xff; 16], u8::MAX, u8::MAX))?,
        _ => t.range::<AggKey>(..)?,
    };
    let mut out: BTreeMap<AggGroup, AggTotal> = BTreeMap::new();
    for entry in range {
        let (k, v) = entry?;
        let key = k.value();
        if !filter.admits(key) {
            continue;
        }
        let a = Agg::from_value(v.value());
        let g = out.entry(group_by.group(key)).or_default();
        g.count += a.count;
        g.access_sum += a.access_sum;
        g.salience_sum += a.salience_sum_at(key.2, now);
    }
    Ok(Some(out))
}

#[cfg(all(test, not(miri)))]
mod tests {
    use super::*;
    use crate::tables::fresh_db;
    use brain_core::{MemoryId, MemoryKind, NamespaceId, SessionId, SpaceId};
    use redb::{Database, ReadableDatabase};

    const DAY: u64 = 86_400 * 1_000_000_000;
    /// Rows are created here; tests read at later times.
    const T0: u64 = crate::salience::LANDMARK_UNIX_NANOS + 100 * DAY;

    fn row(slot: u64, ns: u32, space: u8, kind: MemoryKind, sal: f32) -> MemoryMetadata {
        let mut sp = [0u8; 16];
        sp[15] = space;
        MemoryMetadata::new_active(
            MemoryId::pack(0, slot, 1),
            NamespaceId::from(ns),
            SpaceId::from(sp),
            SessionId(1),
            slot,
            1,
            kind,
            [0; 16],
            sal,
            10,
            T0,
        )
    }

    fn assert_consistent(db: &Database, now: u64) {
        let rtxn = db.begin_read().unwrap();
        let r = verify(&rtxn, now).unwrap();
        assert!(r.is_clean(), "{r:?}");
    }

    /// Totals straight from the rows, as `query` must report them.
    fn brute(
        db: &Database,
        filter: &AggFilter,
        by: AggGroupBy,
        now: u64,
    ) -> BTreeMap<AggGroup, AggTotal> {
        let rtxn = db.begin_read().unwrap();
        let mut out: BTreeMap<AggGroup, AggTotal> = BTreeMap::new();
        for e in rtxn.open_table(MEMORIES_TABLE).unwrap().iter().unwrap() {
            let m = e.unwrap().1.value();
            let key = (m.namespace_id, m.space_id_bytes, m.kind, status_of(&m));
            if filter.admits(key) {
                let g = out.entry(by.group(key)).or_default();
                g.count += 1;
                g.access_sum += i64::from(m.access_count);
                g.salience_sum += f64::from(m.salience_at(now));
            }
        }
        out
    }

    fn assert_query_exact(db: &Database, now: u64) {
        let by = AggGroupBy {
            namespace: true,
            kind: true,
            status: true,
            ..Default::default()
        };
        let filter = AggFilter {
            statuses: vec![status::ACTIVE, status::TOMBSTONED, status::HARD_FORGOTTEN],
            ..Default::default()
        };
        let got = query(&db.begin_read().unwrap(), &filter, by, now)
            .unwrap()
            .expect("ready");
        let want = brute(db, &filter, by, now);
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            want.keys().collect::<Vec<_>>()
        );
        for (k, w) in &want {
            let g = got[k];
            assert_eq!((g.count, g.access_sum), (w.count, w.access_sum), "{k:?}");
            // f32 per-row reads vs f64 sums: within 1e-6 per row.
            assert!(
                (g.salience_sum - w.salience_sum).abs() <= 1e-6 * w.count as f64,
                "{k:?}: {} vs {}",
                g.salience_sum,
                w.salience_sum
            );
        }
    }

    /// Small deterministic PRNG so the test needs no extra dependency.
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

    /// The core guarantee: after any mix of encode / salience change /
    /// kind change / access bump / tombstone / hard forget / restore /
    /// delete, through both immediate and batched writes, interleaved
    /// with time passing and floor sweeps, the counters equal a full
    /// recompute and queries equal a brute-force read at every step.
    #[test]
    fn counters_match_full_scan_after_random_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let mut rng = Lcg(42);
        let kinds = [
            MemoryKind::Episodic,
            MemoryKind::Semantic,
            MemoryKind::Consolidated,
        ];
        let mut now = T0;

        for round in 0..60 {
            now += rng.next(20) * DAY;
            let wtxn = db.begin_write().unwrap();
            // Every third round, immediate writes join a txn batch.
            let txn_batch = (round % 3 == 1).then(|| begin_batch(&wtxn));
            {
                let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
                let mut batch = AggDeltas::default();
                for _ in 0..25 {
                    let slot = rng.next(60);
                    let id = MemoryId::pack(0, slot, 1).to_be_bytes();
                    let existing = mem.get(&id).unwrap().map(|g| g.value());
                    let deferred = round % 2 == 0;
                    let new = match (existing, rng.next(8)) {
                        (None, _) | (Some(_), 0) => Some(row(
                            slot,
                            rng.next(3) as u32,
                            rng.next(4) as u8,
                            kinds[rng.next(3) as usize],
                            rng.next(1000) as f32 / 1000.0,
                        )),
                        (Some(mut r), 1) => {
                            r.set_salience(rng.next(1000) as f32 / 1000.0, now);
                            Some(r)
                        }
                        (Some(mut r), 2) => {
                            r.kind = rng.next(3) as u8;
                            Some(r)
                        }
                        (Some(mut r), 3) => {
                            // Access boost: re-anchor at now.
                            r.access_count += 1;
                            let s = (r.salience_at(now) * 1.1).min(1.0);
                            r.set_salience(s, now);
                            Some(r)
                        }
                        (Some(mut r), 4) => {
                            r.flags &= !flags::ACTIVE;
                            Some(r)
                        }
                        (Some(mut r), 5) => {
                            r.flags = (r.flags & !flags::ACTIVE) | flags::HARD_FORGOTTEN;
                            Some(r)
                        }
                        (Some(mut r), 6) => {
                            r.flags |= flags::ACTIVE;
                            r.flags &= !flags::HARD_FORGOTTEN;
                            Some(r)
                        }
                        (Some(mut r), _) => {
                            // Edge-count-only change: no counter delta.
                            r.edges_out_count += 1;
                            Some(r)
                        }
                    };
                    let delete = rng.next(10) == 0;
                    match (delete, new, deferred) {
                        (true, _, true) => {
                            remove_memory_deferred(&mut mem, id, &mut batch).unwrap();
                        }
                        (true, _, false) => {
                            remove_memory(&wtxn, &mut mem, id).unwrap();
                        }
                        (false, Some(n), true) => {
                            put_memory_deferred(&mut mem, id, &n, &mut batch).unwrap();
                        }
                        (false, Some(n), false) => {
                            put_memory(&wtxn, &mut mem, id, &n).unwrap();
                        }
                        (false, None, _) => {}
                    }
                }
                batch.flush(&wtxn).unwrap();
            }
            if let Some(b) = txn_batch {
                b.finish(&wtxn).unwrap();
            }
            wtxn.commit().unwrap();
            assert_consistent(&db, now);
            assert_query_exact(&db, now);
        }
    }

    /// A txn batch applies all its rows' counter changes at finish; an
    /// abandoned one (txn dropped on an error path) leaves nothing behind,
    /// and the next txn is unaffected.
    #[test]
    fn txn_batch_applies_at_finish_and_discards_when_abandoned() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let count = |db: &Database| {
            query(
                &db.begin_read().unwrap(),
                &AggFilter::default(),
                AggGroupBy::default(),
                T0,
            )
            .unwrap()
            .unwrap()
            .get(&AggGroup::default())
            .map_or(0, |t| t.count)
        };
        {
            let wtxn = db.begin_write().unwrap();
            let _b = begin_batch(&wtxn);
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let r = row(1, 1, 1, MemoryKind::Episodic, 0.5);
            put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
            // dropped without finish or commit
        }
        assert_eq!(count(&db), 0);
        let wtxn = db.begin_write().unwrap();
        let b = begin_batch(&wtxn);
        let nested = begin_batch(&wtxn);
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for s in 0..5 {
                let r = row(s, 1, 1, MemoryKind::Episodic, 0.5);
                put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
            }
            remove_memory(&wtxn, &mut mem, MemoryId::pack(0, 2, 1).to_be_bytes()).unwrap();
        }
        nested.finish(&wtxn).unwrap(); // joins the outer batch: no-op
        b.finish(&wtxn).unwrap();
        wtxn.commit().unwrap();
        assert_eq!(count(&db), 4);
        assert_consistent(&db, T0);
        // Next txn writes immediately again.
        let wtxn = db.begin_write().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let r = row(9, 1, 1, MemoryKind::Episodic, 0.5);
            put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
        }
        wtxn.commit().unwrap();
        assert_eq!(count(&db), 5);
        assert_consistent(&db, T0);
    }

    /// Salience sums follow decay with no writes at all, and the sweep
    /// moves rows to their floor without changing any answer.
    #[test]
    fn sums_decay_over_time_without_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for s in 0..30u64 {
                let kind = [
                    MemoryKind::Episodic,
                    MemoryKind::Semantic,
                    MemoryKind::Consolidated,
                ][(s % 3) as usize];
                let r = row(s, 1, 1, kind, 0.1 + (s as f32) * 0.03);
                put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
            }
        }
        wtxn.commit().unwrap();

        let filter = AggFilter::default();
        let total = |now| {
            query(
                &db.begin_read().unwrap(),
                &filter,
                AggGroupBy::default(),
                now,
            )
            .unwrap()
            .unwrap()[&AggGroup::default()]
        };
        let start = total(T0).salience_sum;
        let month = total(T0 + 30 * DAY).salience_sum;
        assert!(month < start, "decays with time: {start} -> {month}");
        for days in [0u64, 1, 30, 200, 2000, 20_000] {
            assert_query_exact(&db, T0 + days * DAY);
        }
        // Far future: everything has approached its floor.
        let far = T0 + 50_000 * DAY;
        assert!(
            (total(far).salience_sum - 30.0 * f64::from(crate::salience::SALIENCE_FLOOR)).abs()
                < 1e-6
        );
        assert_query_exact(&db, far);
        assert_consistent(&db, far);
    }

    #[test]
    fn query_filters_and_groups() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            let rows = [
                row(1, 7, 1, MemoryKind::Semantic, 0.5),
                row(2, 7, 1, MemoryKind::Semantic, 0.9),
                row(3, 7, 2, MemoryKind::Episodic, 0.1),
                row(4, 8, 1, MemoryKind::Semantic, 1.0),
            ];
            for r in &rows {
                put_memory(&wtxn, &mut mem, r.memory_id_bytes, r).unwrap();
            }
            let mut dead = row(5, 7, 1, MemoryKind::Semantic, 0.3);
            dead.flags &= !flags::ACTIVE;
            put_memory(&wtxn, &mut mem, dead.memory_id_bytes, &dead).unwrap();
        }
        wtxn.commit().unwrap();

        let rtxn = db.begin_read().unwrap();
        // Namespace 7, active only (tombstoned row excluded), by kind.
        let by_kind = query(
            &rtxn,
            &AggFilter {
                namespace_id: Some(7),
                ..Default::default()
            },
            AggGroupBy {
                kind: true,
                ..Default::default()
            },
            T0,
        )
        .unwrap()
        .expect("counters ready");
        let sem = by_kind[&AggGroup {
            kind: Some(1),
            ..Default::default()
        }];
        assert_eq!(sem.count, 2);
        assert!((sem.avg_salience().unwrap() - 0.7).abs() < 1e-6);
        let epi = by_kind[&AggGroup {
            kind: Some(0),
            ..Default::default()
        }];
        assert_eq!(epi.count, 1);

        // Grand total across namespaces, including tombstoned.
        let all = query(
            &rtxn,
            &AggFilter {
                statuses: vec![status::ACTIVE, status::TOMBSTONED],
                ..Default::default()
            },
            AggGroupBy::default(),
            T0,
        )
        .unwrap()
        .expect("counters ready");
        assert_eq!(all[&AggGroup::default()].count, 5);
    }

    fn raw_insert(db: &Database, rows: &[MemoryMetadata]) {
        // As a pre-upgrade binary (or a buggy path) would write: no counters.
        let wtxn = db.begin_write().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for r in rows {
                mem.insert(&r.memory_id_bytes, r).unwrap();
            }
        }
        wtxn.commit().unwrap();
    }

    /// An online build over rows written before the counters existed,
    /// interleaved with writes on both sides of the cursor, ends with
    /// exact counters; partial state is consistent after every step, and
    /// queries refuse to answer until the build finishes.
    #[test]
    fn online_build_is_exact_under_concurrent_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let legacy: Vec<_> = (0..100)
            .map(|s| row(s, (s % 3) as u32, (s % 4) as u8, MemoryKind::Semantic, 0.25))
            .collect();
        raw_insert(&db, &legacy);

        let wtxn = db.begin_write().unwrap();
        assert!(backfill_if_missing(&wtxn).unwrap());
        wtxn.commit().unwrap();
        assert_consistent(&db, T0); // nothing counted yet, nothing stored
        let rtxn = db.begin_read().unwrap();
        assert!(
            query(&rtxn, &AggFilter::default(), AggGroupBy::default(), T0)
                .unwrap()
                .is_none()
        );
        drop(rtxn);

        let mut rng = Lcg(5);
        let mut steps = 0;
        let mut now = T0;
        loop {
            now += DAY;
            // Writes anywhere in the id space, including new rows.
            let wtxn = db.begin_write().unwrap();
            {
                let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
                for _ in 0..10 {
                    let slot = rng.next(130);
                    let id = MemoryId::pack(0, slot, 1).to_be_bytes();
                    if rng.next(6) == 0 {
                        remove_memory(&wtxn, &mut mem, id).unwrap();
                    } else {
                        let mut r = row(
                            slot,
                            rng.next(3) as u32,
                            rng.next(4) as u8,
                            MemoryKind::Episodic,
                            0.5,
                        );
                        r.set_salience(rng.next(1000) as f32 / 1000.0, now);
                        put_memory(&wtxn, &mut mem, id, &r).unwrap();
                    }
                }
            }
            wtxn.commit().unwrap();
            assert_consistent(&db, now);

            let wtxn = db.begin_write().unwrap();
            let step = build_step(&wtxn, 7).unwrap();
            wtxn.commit().unwrap();
            assert_consistent(&db, now);
            steps += 1;
            if let BuildStep::Finished { .. } = step {
                break;
            }
            assert!(steps < 100, "build must terminate");
        }
        let rtxn = db.begin_read().unwrap();
        assert!(build_cursor(&rtxn).unwrap().is_none());
        drop(rtxn);
        assert_query_exact(&db, now);
        let wtxn = db.begin_write().unwrap();
        assert!(
            !backfill_if_missing(&wtxn).unwrap(),
            "second call is a no-op"
        );
        assert_eq!(build_step(&wtxn, 7).unwrap(), BuildStep::Idle);
        wtxn.commit().unwrap();
    }

    /// Drift found on one snapshot is still repaired correctly after more
    /// writes land in between.
    #[test]
    fn corrections_repair_drift_after_intervening_writes() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_db(&dir);
        let wtxn = db.begin_write().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for s in 0..20 {
                let r = row(s, 1, 1, MemoryKind::Semantic, 0.5);
                put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
            }
        }
        wtxn.commit().unwrap();
        // A write that bypasses the counters.
        raw_insert(&db, &[row(500, 1, 1, MemoryKind::Semantic, 0.9)]);
        let report = verify(&db.begin_read().unwrap(), T0).unwrap();
        assert_eq!(report.drift.len(), 1);

        // More correct writes, including to the drifted bucket.
        let wtxn = db.begin_write().unwrap();
        {
            let mut mem = wtxn.open_table(MEMORIES_TABLE).unwrap();
            for s in 20..30 {
                let r = row(s, 1, 1, MemoryKind::Semantic, 0.1);
                put_memory(&wtxn, &mut mem, r.memory_id_bytes, &r).unwrap();
            }
            remove_memory(&wtxn, &mut mem, MemoryId::pack(0, 3, 1).to_be_bytes()).unwrap();
        }
        apply_corrections(&wtxn, &report.drift).unwrap();
        wtxn.commit().unwrap();
        assert_consistent(&db, T0);
    }
}
