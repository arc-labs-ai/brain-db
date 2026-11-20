//! Tier 1: a consistent Parquet snapshot of memory metadata.
//!
//! **Layout.** A snapshot is [`BUCKETS`] files, one per hash bucket of
//! `(namespace, space)` (`snapshot_changes::bucket_of`), so a query for one
//! space opens one file — about 1/16 of the data. Hash buckets rather than
//! one file per tenant: per-tenant files would be tiny and numerous.
//!
//! **Full export.** One redb read txn (a point-in-time snapshot), one
//! sequential pass over `MEMORIES_TABLE` split into id ranges scanned in
//! parallel. Rows are routed to their bucket and written in row groups
//! sorted by `(namespace, space, created_at)`, so the Parquet page index
//! lets a space / time filter read only matching pages. Every memory is
//! emitted exactly once by construction. Used at first start (or when
//! change tracking was off); afterwards snapshots move forward
//! incrementally:
//!
//! **Delta.** A snapshot records the change-log sequence it covers
//! (`brain_metadata::tables::snapshot_changes`). Each later export writes
//! one *delta* file: the current state of every memory changed since then,
//! deleted ones marked, each tagged with its bucket. Queries read
//! `bucket file − changed ids + the bucket's delta rows`.
//!
//! **Compaction.** Once the delta passes a fraction of the snapshot, every
//! bucket file is rewritten from the old file plus the delta (DataFusion:
//! anti-join, union, sort) — Parquet in, Parquet out. That never reads the
//! live database, so it holds no long redb read txn, and leaves each bucket
//! fully sorted.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use brain_metadata::tables::agg::status_of;
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};
use brain_metadata::tables::snapshot_changes::{self, bucket_of, SNAPSHOT_BUCKETS};
use brain_metadata::MetadataDb;
use datafusion::arrow::array::{
    ArrayRef, BooleanArray, FixedSizeBinaryArray, Float32Array, Float64Array, Int64Array,
    UInt32Array, UInt64Array, UInt8Array,
};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::common::JoinType;
use datafusion::parquet::arrow::ArrowWriter;
use datafusion::parquet::basic::Compression;
use datafusion::parquet::file::metadata::KeyValue;
use datafusion::parquet::file::properties::WriterProperties;
use datafusion::parquet::file::reader::{FileReader, SerializedFileReader};
use datafusion::prelude::{col, lit, DataFrame, Expr, ParquetReadOptions, SessionContext};
use futures_util::StreamExt;
use redb::ReadableTable;

use crate::AnalyticsError;

/// Files per snapshot.
pub const BUCKETS: usize = SNAPSHOT_BUCKETS as usize;

/// Rows per Parquet row group in a compacted file.
const ROW_GROUP_ROWS: usize = 1024 * 1024;
/// Rows a full export buffers per bucket before sorting and writing one
/// row group (bounded memory: about `BUCKETS` × this × ~60 bytes).
const BUCKET_FLUSH_ROWS: usize = 256 * 1024;
/// Rows per chunk a scan thread hands the writer.
const SCAN_CHUNK_ROWS: usize = 64 * 1024;

const PREFIX: &str = "memories-";
const DELTA_PREFIX: &str = "delta-";
const SUFFIX: &str = ".parquet";
/// Footer key holding the change-log sequence a file covers.
const SEQ_KEY: &str = "brain.change_seq";

/// A finished snapshot (one file per bucket) or delta (one file).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    /// The first file (bucket 0 of a snapshot, or the delta file).
    pub path: PathBuf,
    /// Every file: one per bucket for a snapshot, one for a delta.
    pub files: Vec<PathBuf>,
    pub rows: u64,
    pub bytes: u64,
    /// Read-txn time: every row is as of this instant.
    pub taken_at_unix_nanos: u64,
    pub elapsed_ms: u64,
    /// Change-log sequence the read txn saw (see the module docs).
    pub change_seq: u64,
}

/// The snapshot's columns. Names are what queries reference.
#[must_use]
pub fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        // Joins a delta to the snapshot.
        Field::new("memory_id", DataType::FixedSizeBinary(16), false),
        Field::new("namespace_id", DataType::UInt32, false),
        Field::new("space_id", DataType::FixedSizeBinary(16), false),
        Field::new("kind", DataType::UInt8, false),
        Field::new("status", DataType::UInt8, false),
        // Salience decays, so the snapshot stores what a query needs to
        // evaluate it at any time, in the same forward-decay form the live
        // counters use (`brain_metadata::salience`): the value at `now` is
        // `salience_floor + salience_weight × 2^(−(now − L) / half_life(kind))`.
        Field::new("salience_weight", DataType::Float64, false),
        Field::new("salience_floor", DataType::Float32, false),
        Field::new("access_count", DataType::UInt32, false),
        Field::new("created_at_unix_nanos", DataType::Int64, false),
        Field::new("session_id", DataType::UInt64, false),
    ]))
}

/// A delta's columns: the snapshot's, plus the memory's bucket and whether
/// it is gone.
#[must_use]
pub fn delta_schema() -> SchemaRef {
    let mut fields: Vec<Field> = schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    fields.push(Field::new("bucket", DataType::UInt8, false));
    fields.push(Field::new("deleted", DataType::Boolean, false));
    Arc::new(Schema::new(fields))
}

/// The snapshot's columns as expressions, in order.
pub(crate) fn columns() -> Vec<Expr> {
    schema().fields().iter().map(|f| col(f.name())).collect()
}

/// The exported fields of one memory.
struct Row {
    id: [u8; 16],
    ns: u32,
    space: [u8; 16],
    created: i64,
    kind: u8,
    status: u8,
    weight: f64,
    floor: f32,
    access: u32,
    session: u64,
}

impl From<&MemoryMetadata> for Row {
    fn from(m: &MemoryMetadata) -> Self {
        Self {
            id: m.memory_id_bytes,
            ns: m.namespace_id,
            space: m.space_id_bytes,
            created: i64::try_from(m.created_at_unix_nanos).unwrap_or(i64::MAX),
            kind: m.kind,
            status: status_of(m),
            weight: m.salience_forward_weight(),
            floor: m.salience_floor(),
            access: m.access_count,
            session: m.session_id,
        }
    }
}

impl Row {
    fn bucket(&self) -> u8 {
        bucket_of(self.ns, &self.space)
    }
}

/// A 16-byte binary column. (`try_from_iter` rejects an empty input; a
/// delta with no live or no deleted rows needs one.)
fn fixed16(values: impl Iterator<Item = [u8; 16]>) -> Result<FixedSizeBinaryArray, AnalyticsError> {
    Ok(FixedSizeBinaryArray::try_from_sparse_iter_with_size(
        values.map(Some),
        16,
    )?)
}

/// Sort rows by `(namespace, space, created_at)` and turn them into a batch.
fn build_batch(mut b: Vec<Row>) -> Result<(RecordBatch, Vec<u8>), AnalyticsError> {
    b.sort_by_key(|r| (r.ns, r.space, r.created));
    let buckets = b.iter().map(Row::bucket).collect();
    let cols: Vec<ArrayRef> = vec![
        Arc::new(fixed16(b.iter().map(|r| r.id))?),
        Arc::new(UInt32Array::from_iter_values(b.iter().map(|r| r.ns))),
        Arc::new(fixed16(b.iter().map(|r| r.space))?),
        Arc::new(UInt8Array::from_iter_values(b.iter().map(|r| r.kind))),
        Arc::new(UInt8Array::from_iter_values(b.iter().map(|r| r.status))),
        Arc::new(Float64Array::from_iter_values(b.iter().map(|r| r.weight))),
        Arc::new(Float32Array::from_iter_values(b.iter().map(|r| r.floor))),
        Arc::new(UInt32Array::from_iter_values(b.iter().map(|r| r.access))),
        Arc::new(Int64Array::from_iter_values(b.iter().map(|r| r.created))),
        Arc::new(UInt64Array::from_iter_values(b.iter().map(|r| r.session))),
    ];
    Ok((RecordBatch::try_new(schema(), cols)?, buckets))
}

type ChunkTx = std::sync::mpsc::SyncSender<Result<Vec<Row>, AnalyticsError>>;

/// Scan memory ids in `[lo, hi)` (`hi = None`: to the end) and send them in
/// chunks.
fn scan_partition(
    rtxn: &redb::ReadTransaction,
    lo: [u8; 16],
    hi: Option<[u8; 16]>,
    tx: &ChunkTx,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<(), AnalyticsError> {
    let memories = rtxn.open_table(MEMORIES_TABLE).map_err(meta_err)?;
    let iter = match hi {
        Some(h) => memories.range(lo..h),
        None => memories.range(lo..),
    }
    .map_err(meta_err)?;
    let mut buf: Vec<Row> = Vec::with_capacity(SCAN_CHUNK_ROWS);
    for (i, entry) in iter.enumerate() {
        if i % 65_536 == 0 && stop() {
            return Err(AnalyticsError::Stopped);
        }
        let (_, v) = entry.map_err(meta_err)?;
        buf.push(Row::from(&v.value()));
        if buf.len() == SCAN_CHUNK_ROWS {
            let full = std::mem::replace(&mut buf, Vec::with_capacity(SCAN_CHUNK_ROWS));
            if tx.send(Ok(full)).is_err() {
                return Ok(()); // writer stopped; its error is reported there
            }
        }
    }
    if !buf.is_empty() {
        let _ = tx.send(Ok(buf));
    }
    Ok(())
}

/// A `[lo, hi)` memory-id range; `hi = None` means to the end.
type IdRange = ([u8; 16], Option<[u8; 16]>);

/// Split the populated id range into `n` contiguous `[lo, hi)` ranges.
fn partition_bounds(
    rtxn: &redb::ReadTransaction,
    n: usize,
) -> Result<Vec<IdRange>, AnalyticsError> {
    let memories = rtxn.open_table(MEMORIES_TABLE).map_err(meta_err)?;
    let first = memories.first().map_err(meta_err)?.map(|(k, _)| k.value());
    let last = memories.last().map_err(meta_err)?.map(|(k, _)| k.value());
    let (Some(first), Some(last)) = (first, last) else {
        return Ok(vec![([0u8; 16], None)]);
    };
    let (lo, hi) = (u128::from_be_bytes(first), u128::from_be_bytes(last));
    let n = n.max(1) as u128;
    let step = (hi - lo) / n;
    if step == 0 {
        return Ok(vec![(first, None)]);
    }
    let cut = |i: u128| (lo + step * i).to_be_bytes();
    Ok((0..n)
        .map(|i| (cut(i), (i + 1 < n).then(|| cut(i + 1))))
        .collect())
}

fn meta_err(e: impl std::fmt::Display) -> AnalyticsError {
    AnalyticsError::Metadata(e.to_string())
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

fn elapsed_ms(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX)
}

// Uncompressed: dictionary + RLE already shrink the low-cardinality
// columns, and salience floats barely compress. Skipping codecs keeps the
// dependency tree small.
fn writer_props(change_seq: u64) -> WriterProperties {
    WriterProperties::builder()
        .set_compression(Compression::UNCOMPRESSED)
        .set_max_row_group_row_count(Some(ROW_GROUP_ROWS))
        .set_key_value_metadata(Some(vec![KeyValue::new(
            SEQ_KEY.to_owned(),
            change_seq.to_string(),
        )]))
        .build()
}

/// Files being written under temp names: renamed into place together by
/// [`Self::commit`], or deleted on drop (an error or a stop part-way).
struct PendingFiles(Vec<(PathBuf, PathBuf)>);

impl PendingFiles {
    fn new(finals: Vec<PathBuf>) -> Self {
        Self(
            finals
                .into_iter()
                .map(|f| (f.with_extension("parquet.tmp"), f))
                .collect(),
        )
    }

    fn tmp(&self, i: usize) -> &Path {
        &self.0[i].0
    }

    /// Fsync and rename every file into place; returns the final paths and
    /// their total size.
    fn commit(mut self) -> Result<(Vec<PathBuf>, u64), AnalyticsError> {
        for (tmp, _) in &self.0 {
            File::open(tmp)?.sync_all()?;
        }
        let mut bytes = 0;
        let mut finals = Vec::with_capacity(self.0.len());
        for (tmp, fin) in std::mem::take(&mut self.0) {
            std::fs::rename(&tmp, &fin)?;
            bytes += std::fs::metadata(&fin)?.len();
            finals.push(fin);
        }
        Ok((finals, bytes))
    }
}

impl Drop for PendingFiles {
    fn drop(&mut self) {
        for (tmp, _) in &self.0 {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

fn snapshot_paths(dir: &Path, taken_at: u64) -> Vec<PathBuf> {
    (0..BUCKETS)
        .map(|b| dir.join(format!("{PREFIX}{taken_at:020}-b{b:02}{SUFFIX}")))
        .collect()
}

/// Write a full snapshot (one file per bucket) into `dir` and return its
/// info. Does not touch older snapshots. `taken_at` is forced above
/// `after` (the previous snapshot's) so names sort in export order even if
/// the wall clock steps back. `stop` is polled every 64K rows per
/// partition; when it fires the export ends with
/// [`AnalyticsError::Stopped`] and leaves no file behind.
///
/// One read txn gives every partition the same point-in-time view;
/// `partitions` threads scan disjoint id ranges of it in parallel and one
/// writer routes their rows into per-bucket sorted row groups.
pub fn export_snapshot(
    db: &MetadataDb,
    dir: &Path,
    partitions: usize,
    after: u64,
    stop: &(dyn Fn() -> bool + Sync),
) -> Result<SnapshotInfo, AnalyticsError> {
    let started = Instant::now();
    let rtxn = db.read_txn().map_err(meta_err)?;
    let change_seq = snapshot_changes::current_seq(&rtxn).map_err(meta_err)?;
    let taken_at = now_nanos().max(after.saturating_add(1));
    let bounds = partition_bounds(&rtxn, partitions)?;

    let pending = PendingFiles::new(snapshot_paths(dir, taken_at));
    let mut writers = (0..BUCKETS)
        .map(|b| {
            ArrowWriter::try_new(
                File::create(pending.tmp(b))?,
                schema(),
                Some(writer_props(change_seq)),
            )
            .map_err(AnalyticsError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;

    let rows = std::thread::scope(|s| -> Result<u64, AnalyticsError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(bounds.len() * 2);
        for &(lo, hi) in &bounds {
            let (tx, rtxn) = (tx.clone(), &rtxn);
            s.spawn(move || {
                if let Err(e) = scan_partition(rtxn, lo, hi, &tx, stop) {
                    let _ = tx.send(Err(e));
                }
            });
        }
        drop(tx);
        let flush = |w: &mut ArrowWriter<File>, buf: Vec<Row>| -> Result<(), AnalyticsError> {
            if !buf.is_empty() {
                w.write(&build_batch(buf)?.0)?;
                w.flush()?; // one sorted row group
            }
            Ok(())
        };
        let mut buffers: Vec<Vec<Row>> = (0..BUCKETS).map(|_| Vec::new()).collect();
        let mut rows = 0u64;
        for chunk in rx {
            for r in chunk? {
                rows += 1;
                let b = usize::from(r.bucket());
                buffers[b].push(r);
                if buffers[b].len() >= BUCKET_FLUSH_ROWS {
                    flush(&mut writers[b], std::mem::take(&mut buffers[b]))?;
                }
            }
        }
        for (w, buf) in writers.iter_mut().zip(buffers) {
            flush(w, buf)?;
        }
        Ok(rows)
    })?;
    for w in writers {
        w.close()?;
    }
    let (files, bytes) = pending.commit()?;
    Ok(SnapshotInfo {
        path: files[0].clone(),
        files,
        rows,
        bytes,
        taken_at_unix_nanos: taken_at,
        elapsed_ms: elapsed_ms(started),
        change_seq,
    })
}

/// Write `delta-<taken_at>.parquet`: the state, as of one read txn, of
/// every memory changed since `snapshot` (deleted ones marked), each with
/// its bucket. `taken_at` is forced above `after`, like
/// [`export_snapshot`].
pub fn export_delta(
    db: &MetadataDb,
    dir: &Path,
    snapshot: &SnapshotInfo,
    after: u64,
) -> Result<SnapshotInfo, AnalyticsError> {
    let started = Instant::now();
    let rtxn = db.read_txn().map_err(meta_err)?;
    let change_seq = snapshot_changes::current_seq(&rtxn).map_err(meta_err)?;
    let ids = snapshot_changes::changed_since(&rtxn, snapshot.change_seq, usize::MAX)
        .map_err(meta_err)?
        .unwrap_or_default();
    let taken_at = now_nanos().max(after.saturating_add(1));
    let memories = rtxn.open_table(MEMORIES_TABLE).map_err(meta_err)?;
    let mut live = Vec::with_capacity(ids.len());
    let mut gone = Vec::new();
    for (id, bucket) in ids {
        match memories.get(id).map_err(meta_err)? {
            Some(g) => live.push(Row::from(&g.value())),
            None => gone.push((id, bucket)),
        }
    }
    drop(memories);
    let (live, live_buckets) = build_batch(live)?;
    let live = with_delta_cols(live, live_buckets, false)?;
    let gone = tombstones(&gone)?;

    let pending = PendingFiles::new(vec![
        dir.join(format!("{DELTA_PREFIX}{taken_at:020}{SUFFIX}"))
    ]);
    let mut writer = ArrowWriter::try_new(
        File::create(pending.tmp(0))?,
        delta_schema(),
        Some(writer_props(change_seq)),
    )?;
    writer.write(&live)?;
    writer.write(&gone)?;
    writer.close()?;
    let (files, bytes) = pending.commit()?;
    Ok(SnapshotInfo {
        path: files[0].clone(),
        files,
        rows: (live.num_rows() + gone.num_rows()) as u64,
        bytes,
        taken_at_unix_nanos: taken_at,
        elapsed_ms: elapsed_ms(started),
        change_seq,
    })
}

/// Delta rows for deleted memories: only `memory_id`, `bucket` and
/// `deleted` matter; the bucket comes from the change log (no row left).
fn tombstones(ids: &[([u8; 16], u8)]) -> Result<RecordBatch, AnalyticsError> {
    let rows = ids
        .iter()
        .map(|&(id, _)| Row {
            id,
            ns: 0,
            space: [0; 16],
            created: 0,
            kind: 0,
            status: 0,
            weight: 0.0,
            floor: 0.0,
            access: 0,
            session: 0,
        })
        .collect();
    // Equal sort keys: `build_batch`'s stable sort keeps `ids`' order.
    let (batch, _) = build_batch(rows)?;
    with_delta_cols(batch, ids.iter().map(|&(_, b)| b).collect(), true)
}

/// Append the delta's `bucket` and `deleted` columns to a snapshot batch.
fn with_delta_cols(
    batch: RecordBatch,
    buckets: Vec<u8>,
    deleted: bool,
) -> Result<RecordBatch, AnalyticsError> {
    let mut cols: Vec<ArrayRef> = batch.columns().to_vec();
    cols.push(Arc::new(UInt8Array::from(buckets)));
    cols.push(Arc::new(BooleanArray::from(vec![
        deleted;
        batch.num_rows()
    ])));
    Ok(RecordBatch::try_new(delta_schema(), cols)?)
}

/// `bucket_file − delta ids + delta live rows`, for one bucket, over the
/// snapshot's columns. Queries and compaction share it.
pub(crate) async fn merged_bucket(
    ctx: &SessionContext,
    file: &Path,
    delta: Option<&Path>,
    bucket: u8,
) -> Result<DataFrame, AnalyticsError> {
    let read = |p: &Path| {
        ctx.read_parquet(
            p.to_string_lossy().into_owned(),
            ParquetReadOptions::default(),
        )
    };
    let base = read(file).await?;
    let Some(delta) = delta else {
        return Ok(base);
    };
    let delta = read(delta).await?.filter(col("bucket").eq(lit(bucket)))?;
    // The (small) delta's ids build the join's hash table.
    let ids = delta
        .clone()
        .select(vec![col("memory_id").alias("delta_id")])?;
    let kept = ids
        .join(
            base,
            JoinType::RightAnti,
            &["delta_id"],
            &["memory_id"],
            None,
        )?
        .select(columns())?;
    let live = delta
        .filter(col("deleted").eq(lit(false)))?
        .select(columns())?;
    Ok(kept.union(live)?)
}

/// Fold `delta` into `snapshot`: rewrite every bucket file as
/// `old − changed + delta`, fully sorted, as a new snapshot covering the
/// delta's change sequence. Reads only Parquet (never the live DB).
pub(crate) async fn compact(
    ctx: &SessionContext,
    dir: &Path,
    snapshot: &SnapshotInfo,
    delta: &SnapshotInfo,
) -> Result<SnapshotInfo, AnalyticsError> {
    let started = Instant::now();
    let taken_at = delta.taken_at_unix_nanos;
    let pending = PendingFiles::new(snapshot_paths(dir, taken_at));
    let mut rows = 0u64;
    for (b, file) in snapshot.files.iter().enumerate() {
        let bucket = u8::try_from(b).expect("bucket fits u8");
        let mut stream = merged_bucket(ctx, file, Some(&delta.path), bucket)
            .await?
            .sort(vec![
                col("namespace_id").sort(true, false),
                col("space_id").sort(true, false),
                col("created_at_unix_nanos").sort(true, false),
            ])?
            .execute_stream()
            .await?;
        let mut w = ArrowWriter::try_new(
            File::create(pending.tmp(b))?,
            schema(),
            Some(writer_props(delta.change_seq)),
        )?;
        while let Some(batch) = stream.next().await {
            let batch = batch?;
            rows += batch.num_rows() as u64;
            // Same columns as the snapshot schema; rebuild under it so the
            // file's schema is exactly the snapshot's.
            w.write(&RecordBatch::try_new(schema(), batch.columns().to_vec())?)?;
        }
        w.close()?;
    }
    let (files, bytes) = pending.commit()?;
    Ok(SnapshotInfo {
        path: files[0].clone(),
        files,
        rows,
        bytes,
        taken_at_unix_nanos: taken_at,
        elapsed_ms: elapsed_ms(started),
        change_seq: delta.change_seq,
    })
}

fn names(dir: &Path) -> Result<Vec<(String, PathBuf)>, AnalyticsError> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
            out.push((name.to_owned(), p.clone()));
        }
    }
    Ok(out)
}

/// Delta files in `dir`, newest first, as `(taken_at, path)`.
fn deltas(dir: &Path) -> Result<Vec<(u64, PathBuf)>, AnalyticsError> {
    let mut out = Vec::new();
    for (name, p) in names(dir)? {
        if let Some(ts) = name
            .strip_prefix(DELTA_PREFIX)
            .and_then(|r| r.strip_suffix(SUFFIX))
            .and_then(|t| t.parse::<u64>().ok())
        {
            out.push((ts, p));
        }
    }
    out.sort_unstable_by_key(|a| std::cmp::Reverse(a.0));
    Ok(out)
}

/// A snapshot's files by bucket, as found on disk (`None`: missing).
type FileSet = Vec<Option<PathBuf>>;

/// Snapshot file sets in `dir`, newest first.
fn snapshots(dir: &Path) -> Result<Vec<(u64, FileSet)>, AnalyticsError> {
    let mut sets: std::collections::BTreeMap<u64, FileSet> = Default::default();
    for (name, p) in names(dir)? {
        let Some(rest) = name
            .strip_prefix(PREFIX)
            .and_then(|r| r.strip_suffix(SUFFIX))
        else {
            continue;
        };
        let Some((ts, b)) = rest.split_once("-b") else {
            continue;
        };
        let (Ok(ts), Ok(b)) = (ts.parse::<u64>(), b.parse::<usize>()) else {
            continue;
        };
        if b < BUCKETS {
            sets.entry(ts).or_insert_with(|| vec![None; BUCKETS])[b] = Some(p);
        }
    }
    Ok(sets.into_iter().rev().collect())
}

/// The newest snapshot in `dir`, if complete and readable. Leftover `.tmp`
/// files from an interrupted export are removed, and so are deltas (the
/// export loop writes a fresh one right away). A damaged or incomplete
/// newest snapshot is removed and `None` returned — derived data, the next
/// export replaces it — rather than falling back to an older one, whose
/// changes the shard may already have pruned from its change log.
pub(crate) fn latest_snapshot(dir: &Path) -> Result<Option<SnapshotInfo>, AnalyticsError> {
    for (name, p) in names(dir)? {
        if name.ends_with(".tmp") {
            let _ = std::fs::remove_file(&p);
        }
    }
    for (_, p) in deltas(dir)? {
        let _ = std::fs::remove_file(p);
    }
    let Some((ts, files)) = snapshots(dir)?.into_iter().next() else {
        return Ok(None);
    };
    let read = || -> Result<SnapshotInfo, AnalyticsError> {
        let files: Vec<PathBuf> = files
            .iter()
            .cloned()
            .collect::<Option<_>>()
            .ok_or_else(|| AnalyticsError::Metadata("snapshot is missing a bucket file".into()))?;
        let (mut rows, mut bytes, mut seq) = (0, 0, None);
        for f in &files {
            let (r, b, s) = read_footer(f)?;
            if seq.is_some_and(|x| x != s) {
                return Err(AnalyticsError::Metadata(
                    "bucket files disagree on change seq".into(),
                ));
            }
            (rows, bytes, seq) = (rows + r, bytes + b, Some(s));
        }
        Ok(SnapshotInfo {
            path: files[0].clone(),
            files,
            rows,
            bytes,
            taken_at_unix_nanos: ts,
            elapsed_ms: 0,
            change_seq: seq.unwrap_or(0),
        })
    };
    match read() {
        Ok(info) => Ok(Some(info)),
        Err(e) => {
            tracing::warn!(error = %e, "removing unreadable analytics snapshot");
            for p in files.into_iter().flatten() {
                let _ = std::fs::remove_file(p);
            }
            Ok(None)
        }
    }
}

/// Remove every snapshot and delta in `dir` (they can't be extended once
/// change tracking was off).
pub(crate) fn remove_all(dir: &Path) -> Result<(), AnalyticsError> {
    let snaps: Vec<PathBuf> = snapshots(dir)?
        .into_iter()
        .flat_map(|(_, f)| f)
        .flatten()
        .collect();
    for p in snaps
        .into_iter()
        .chain(deltas(dir)?.into_iter().map(|(_, p)| p))
    {
        std::fs::remove_file(p)?;
    }
    Ok(())
}

fn read_footer(path: &Path) -> Result<(u64, u64, u64), AnalyticsError> {
    let file = File::open(path)?;
    let bytes = file.metadata()?.len();
    let reader = SerializedFileReader::new(file)?;
    let meta = reader.metadata().file_metadata();
    let rows = meta.num_rows();
    let seq = meta
        .key_value_metadata()
        .and_then(|kv| kv.iter().find(|k| k.key == SEQ_KEY))
        .and_then(|k| k.value.as_deref()?.parse::<u64>().ok())
        .ok_or_else(|| AnalyticsError::Metadata(format!("{SEQ_KEY} missing from footer")))?;
    Ok((u64::try_from(rows).unwrap_or(0), bytes, seq))
}

/// Keep the newest `keep` snapshots and deltas (and always the files in
/// `current`), delete the rest. The ones before the current stay so a
/// query that started on them can finish.
pub(crate) fn prune_old_snapshots(
    dir: &Path,
    keep: usize,
    current: &[PathBuf],
) -> Result<(), AnalyticsError> {
    let old_snaps: Vec<PathBuf> = snapshots(dir)?
        .into_iter()
        .skip(keep)
        .flat_map(|(_, f)| f)
        .flatten()
        .collect();
    let old_deltas = deltas(dir)?.into_iter().skip(keep).map(|(_, p)| p);
    for p in old_snaps.into_iter().chain(old_deltas) {
        if !current.contains(&p) {
            let _ = std::fs::remove_file(p);
        }
    }
    Ok(())
}
