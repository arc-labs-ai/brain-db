//! Tier 2: aggregate queries over the current snapshot with DataFusion.

use datafusion::arrow::array::{Array, ArrayRef, AsArray, RecordBatch};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{DataType, Float64Type, UInt64Type};
use datafusion::common::ScalarValue;
use datafusion::functions_aggregate::expr_fn::{count, max, min, sum};
use datafusion::prelude::{col, lit, when, Expr, SessionContext};

use brain_metadata::salience::forward_scale;

use brain_metadata::tables::snapshot_changes::bucket_of;

use crate::export::merged_bucket;
use crate::{AnalyticsError, CurrentSnapshot, Inner};

const DAY_NANOS: i64 = 86_400_000_000_000;

/// One aggregate request, always scoped to a single `(namespace, space)`.
#[derive(Debug, Clone, Default)]
pub struct ScanQuery {
    pub namespace_id: u32,
    pub space_id: [u8; 16],
    /// Empty = all kinds.
    pub kinds: Vec<u8>,
    /// Empty = active only (status 0).
    pub statuses: Vec<u8>,
    pub salience_min: Option<f32>,
    pub salience_max: Option<f32>,
    /// Inclusive `created_at` bounds, unix nanos.
    pub created_from_unix_nanos: Option<u64>,
    pub created_to_unix_nanos: Option<u64>,
    pub group_by_kind: bool,
    pub group_by_status: bool,
    /// Bucket by UTC day of `created_at`.
    pub group_by_day: bool,
    /// Time salience is evaluated at (it decays), unix nanos; 0 = now.
    pub at_unix_nanos: u64,
}

/// One result group; grouping fields are `None` when not grouped on.
#[derive(Debug, Clone, PartialEq)]
pub struct ScanGroup {
    pub kind: Option<u8>,
    pub status: Option<u8>,
    pub day_start_unix_nanos: Option<u64>,
    pub count: u64,
    pub salience_sum: f64,
    pub salience_min: Option<f64>,
    pub salience_max: Option<f64>,
    pub access_sum: u64,
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub groups: Vec<ScanGroup>,
    /// Every row counted is as of this instant.
    pub snapshot_taken_at_unix_nanos: u64,
}

pub(crate) async fn run(inner: &Inner, q: ScanQuery) -> Result<ScanResult, AnalyticsError> {
    let limit = inner.cfg.query_timeout;
    // The timeout covers waiting for a permit too, and the snapshot is
    // picked only once the permit is held: a query never starts on a file
    // that queued long enough to be pruned.
    tokio::time::timeout(limit, async {
        let _permit = inner
            .permits
            .acquire()
            .await
            .map_err(|_| AnalyticsError::Stopped)?;
        let snap: CurrentSnapshot = inner
            .current
            .lock()
            .expect("snapshot lock")
            .clone()
            .ok_or(AnalyticsError::NoSnapshot)?;
        let mut q = q;
        if q.at_unix_nanos == 0 {
            q.at_unix_nanos = brain_metadata::salience::now_unix_nanos();
        }
        execute(&inner.ctx, &snap, &q).await
    })
    .await
    .map_err(|_| AnalyticsError::Timeout(limit))?
}

fn u64_nanos(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

/// Each row's salience at `at` (see the export schema): its floor plus its
/// forward weight times its kind's factor, computed once per query.
fn salience_at(at: u64) -> Expr {
    let scale = when(col("kind").eq(lit(1u8)), lit(forward_scale(1, at)))
        .when(col("kind").eq(lit(2u8)), lit(forward_scale(2, at)))
        .otherwise(lit(forward_scale(0, at)))
        .expect("case expr");
    cast_f64(col("salience_floor")) + col("salience_weight") * scale
}

fn cast_f64(e: Expr) -> Expr {
    datafusion::prelude::cast(e, DataType::Float64)
}

fn predicate(q: &ScanQuery) -> Expr {
    let mut p = col("namespace_id")
        .eq(lit(q.namespace_id))
        .and(col("space_id").eq(lit(ScalarValue::FixedSizeBinary(
            16,
            Some(q.space_id.to_vec()),
        ))));
    let statuses = if q.statuses.is_empty() {
        vec![0u8]
    } else {
        q.statuses.clone()
    };
    p = p.and(col("status").in_list(statuses.into_iter().map(lit).collect(), false));
    if !q.kinds.is_empty() {
        p = p.and(col("kind").in_list(q.kinds.iter().copied().map(lit).collect(), false));
    }
    if let Some(v) = q.salience_min {
        p = p.and(salience_at(q.at_unix_nanos).gt_eq(lit(f64::from(v))));
    }
    if let Some(v) = q.salience_max {
        p = p.and(salience_at(q.at_unix_nanos).lt_eq(lit(f64::from(v))));
    }
    if let Some(v) = q.created_from_unix_nanos {
        p = p.and(col("created_at_unix_nanos").gt_eq(lit(u64_nanos(v))));
    }
    if let Some(v) = q.created_to_unix_nanos {
        p = p.and(col("created_at_unix_nanos").lt_eq(lit(u64_nanos(v))));
    }
    p
}

async fn execute(
    ctx: &SessionContext,
    snap: &CurrentSnapshot,
    q: &ScanQuery,
) -> Result<ScanResult, AnalyticsError> {
    // One space lives in one bucket file.
    let bucket = bucket_of(q.namespace_id, &q.space_id);
    let rows = merged_bucket(
        ctx,
        &snap.base.files[usize::from(bucket)],
        snap.delta.as_ref().map(|d| d.path.as_path()),
        bucket,
    )
    .await?;
    let df = rows.filter(predicate(q))?;

    let mut group: Vec<Expr> = Vec::new();
    if q.group_by_kind {
        group.push(col("kind"));
    }
    if q.group_by_status {
        group.push(col("status"));
    }
    if q.group_by_day {
        group.push((col("created_at_unix_nanos") / lit(DAY_NANOS)).alias("day"));
    }
    let aggs = vec![
        count(lit(1u8)).alias("n"),
        sum(salience_at(q.at_unix_nanos)).alias("salience_sum"),
        min(salience_at(q.at_unix_nanos)).alias("salience_min"),
        max(salience_at(q.at_unix_nanos)).alias("salience_max"),
        sum(col("access_count")).alias("access_sum"),
    ];
    let batches = df.aggregate(group, aggs)?.collect().await?;

    let mut groups = Vec::new();
    for b in &batches {
        let n = as_u64(b, "n")?;
        let s_sum = as_f64(b, "salience_sum")?;
        let s_min = as_f64(b, "salience_min")?;
        let s_max = as_f64(b, "salience_max")?;
        let acc = as_u64(b, "access_sum")?;
        let kind = q.group_by_kind.then(|| as_u64(b, "kind")).transpose()?;
        let status = q.group_by_status.then(|| as_u64(b, "status")).transpose()?;
        let day = q.group_by_day.then(|| as_u64(b, "day")).transpose()?;
        for i in 0..b.num_rows() {
            let opt_f = |a: &ArrayRef| {
                let a = a.as_primitive::<Float64Type>();
                (!a.is_null(i)).then(|| a.value(i))
            };
            let opt_u = |a: &ArrayRef| {
                let a = a.as_primitive::<UInt64Type>();
                (!a.is_null(i)).then(|| a.value(i))
            };
            groups.push(ScanGroup {
                kind: kind.as_ref().and_then(opt_u).map(|v| v as u8),
                status: status.as_ref().and_then(opt_u).map(|v| v as u8),
                day_start_unix_nanos: day.as_ref().and_then(opt_u).map(|d| d * DAY_NANOS as u64),
                count: opt_u(&n).unwrap_or(0),
                salience_sum: opt_f(&s_sum).unwrap_or(0.0),
                salience_min: opt_f(&s_min),
                salience_max: opt_f(&s_max),
                access_sum: opt_u(&acc).unwrap_or(0),
            });
        }
    }
    groups.sort_by_key(|g| (g.kind, g.status, g.day_start_unix_nanos));
    Ok(ScanResult {
        groups,
        snapshot_taken_at_unix_nanos: snap.as_of_unix_nanos(),
    })
}

fn column(b: &RecordBatch, name: &str, to: &DataType) -> Result<ArrayRef, AnalyticsError> {
    let a = b.column_by_name(name).ok_or_else(|| {
        AnalyticsError::Engine(datafusion::error::DataFusionError::Internal(format!(
            "missing result column {name}"
        )))
    })?;
    Ok(cast(a, to)?)
}

fn as_u64(b: &RecordBatch, name: &str) -> Result<ArrayRef, AnalyticsError> {
    column(b, name, &DataType::UInt64)
}

fn as_f64(b: &RecordBatch, name: &str) -> Result<ArrayRef, AnalyticsError> {
    column(b, name, &DataType::Float64)
}
