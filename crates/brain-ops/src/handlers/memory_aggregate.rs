//! `MEMORY_AGGREGATE` handler — aggregates over the caller's
//! `(namespace, space)` memories, routed to the cheapest source that can
//! answer:
//!
//! - **Counters** (`brain_metadata::tables::agg`): filters/groups only by
//!   kind and status, COUNT / SUM / AVG. Exact, live, a few row reads.
//! - **Snapshot** (`brain_analytics`): anything else — salience or time
//!   filters, per-day grouping, MIN / MAX. A DataFusion scan of the latest
//!   columnar snapshot, on the analytics runtime, as of the snapshot time.
//!
//! Every request is logged under target `brain::aggregate` with its shape,
//! source and latency, so operators can see which queries miss the
//! counters and whether a new counter dimension would pay off.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use brain_analytics::{AnalyticsError, ScanQuery};
use brain_metadata::tables::agg::{self, AggFilter, AggGroupBy};
use brain_protocol::{
    AggregateSourceWire, MemoryAggregateGroup, MemoryAggregateRequest, MemoryAggregateResponse,
    MemoryKindWire, MemoryStatusWire,
};

use crate::context::OpsContext;
use crate::error::OpError;

/// Per-shard `MEMORY_AGGREGATE` counters for `/metrics`.
#[derive(Debug, Default)]
pub struct AggregateMetrics {
    /// Requests answered from the live counters.
    pub counters_total: AtomicU64,
    /// Requests answered from the analytics snapshot.
    pub snapshot_total: AtomicU64,
    /// Requests that failed (any source).
    pub errors_total: AtomicU64,
}

pub async fn handle_memory_aggregate(
    req: MemoryAggregateRequest,
    ctx: &OpsContext,
) -> Result<MemoryAggregateResponse, OpError> {
    if let (Some(lo), Some(hi)) = (req.salience_min, req.salience_max) {
        if lo > hi {
            return Err(OpError::InvalidRequest(
                "salience_min must be <= salience_max".into(),
            ));
        }
    }
    if let (Some(lo), Some(hi)) = (req.created_from_unix_nanos, req.created_to_unix_nanos) {
        if lo > hi {
            return Err(OpError::InvalidRequest(
                "created_from_unix_nanos must be <= created_to_unix_nanos".into(),
            ));
        }
    }

    let started = Instant::now();
    let needs_scan = needs_scan(&req);
    let result = if needs_scan {
        from_snapshot(&req, ctx).await
    } else {
        from_counters(&req, ctx)
    };
    let m = &ctx.aggregate_metrics;
    match (&result, needs_scan) {
        (Err(_), _) => m.errors_total.fetch_add(1, Ordering::Relaxed),
        (Ok(_), true) => m.snapshot_total.fetch_add(1, Ordering::Relaxed),
        (Ok(_), false) => m.counters_total.fetch_add(1, Ordering::Relaxed),
    };
    tracing::info!(
        target: "brain::aggregate",
        source = if needs_scan { "snapshot" } else { "counters" },
        kinds = req.kinds.len(),
        statuses = req.statuses.len(),
        group_by_kind = req.group_by_kind,
        group_by_status = req.group_by_status,
        group_by_day = req.group_by_day,
        salience_filter = req.salience_min.is_some() || req.salience_max.is_some(),
        time_filter = req.created_from_unix_nanos.is_some() || req.created_to_unix_nanos.is_some(),
        min_max = req.include_min_max,
        groups = result.as_ref().map_or(0, |r| r.groups.len()),
        ok = result.is_ok(),
        elapsed_us = u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX),
        "memory aggregate"
    );
    result
}

/// True when the counters can't answer and a snapshot scan is required.
fn needs_scan(req: &MemoryAggregateRequest) -> bool {
    req.group_by_day
        || req.include_min_max
        || req.salience_min.is_some()
        || req.salience_max.is_some()
        || req.created_from_unix_nanos.is_some()
        || req.created_to_unix_nanos.is_some()
}

fn scope(ctx: &OpsContext) -> (u32, [u8; 16]) {
    (
        u32::from(ctx.executor.caller_namespace),
        ctx.executor.caller_space.into(),
    )
}

fn from_counters(
    req: &MemoryAggregateRequest,
    ctx: &OpsContext,
) -> Result<MemoryAggregateResponse, OpError> {
    let (ns, space) = scope(ctx);
    // Always pinned to the caller's own scope: a tenant can only ever
    // aggregate its own memories.
    let filter = AggFilter {
        namespace_id: Some(ns),
        space_id: Some(space),
        // Both wire enums are `#[repr(u8)]` with the storage discriminants.
        kinds: req.kinds.iter().map(|k| *k as u8).collect(),
        statuses: req.statuses.iter().map(|s| *s as u8).collect(),
    };
    let group_by = AggGroupBy {
        kind: req.group_by_kind,
        status: req.group_by_status,
        ..AggGroupBy::default()
    };

    let rtxn = ctx
        .executor
        .metadata
        .read_txn()
        .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;
    let now = brain_metadata::salience::now_unix_nanos();
    let mut totals = agg::query(&rtxn, &filter, group_by, now)
        .map_err(|e| OpError::Internal(format!("aggregate counters: {e}")))?
        .ok_or_else(|| {
            OpError::RetrievalUnavailable(
                "aggregate counters are still being built after an upgrade; retry shortly".into(),
            )
        })?;

    // An ungrouped query always answers with one total, even when zero.
    if !req.group_by_kind && !req.group_by_status && totals.is_empty() {
        totals.insert(agg::AggGroup::default(), agg::AggTotal::default());
    }

    let groups = totals
        .into_iter()
        .map(|(g, a)| MemoryAggregateGroup {
            kind: g.kind.map(kind_wire),
            status: g.status.map(status_wire),
            day_start_unix_nanos: None,
            count: u64::try_from(a.count).unwrap_or(0),
            salience_sum: a.salience_sum,
            salience_avg: a.avg_salience(),
            salience_min: None,
            salience_max: None,
            access_count_sum: u64::try_from(a.access_sum).unwrap_or(0),
        })
        .collect();
    Ok(MemoryAggregateResponse {
        groups,
        source: AggregateSourceWire::Counters,
        as_of_unix_nanos: 0,
    })
}

async fn from_snapshot(
    req: &MemoryAggregateRequest,
    ctx: &OpsContext,
) -> Result<MemoryAggregateResponse, OpError> {
    let Some(analytics) = ctx.analytics.as_ref() else {
        return Err(OpError::CapabilityNotEnabled {
            capability: "analytics (salience/time filters, per-day grouping, MIN/MAX)",
        });
    };
    let (namespace_id, space_id) = scope(ctx);
    let q = ScanQuery {
        namespace_id,
        space_id,
        kinds: req.kinds.iter().map(|k| *k as u8).collect(),
        statuses: req.statuses.iter().map(|s| *s as u8).collect(),
        salience_min: req.salience_min,
        salience_max: req.salience_max,
        created_from_unix_nanos: req.created_from_unix_nanos,
        created_to_unix_nanos: req.created_to_unix_nanos,
        group_by_kind: req.group_by_kind,
        group_by_status: req.group_by_status,
        group_by_day: req.group_by_day,
        at_unix_nanos: brain_metadata::salience::now_unix_nanos(),
    };
    let res = analytics.query(q).await.map_err(|e| match e {
        AnalyticsError::NoSnapshot => OpError::RetrievalUnavailable(e.to_string()),
        AnalyticsError::Timeout(_) => OpError::Overloaded(e.to_string()),
        other => OpError::Internal(format!("analytics: {other}")),
    })?;
    let groups = res
        .groups
        .into_iter()
        .map(|g| MemoryAggregateGroup {
            kind: g.kind.map(kind_wire),
            status: g.status.map(status_wire),
            day_start_unix_nanos: g.day_start_unix_nanos,
            count: g.count,
            salience_sum: g.salience_sum,
            #[allow(clippy::cast_precision_loss)]
            salience_avg: (g.count > 0).then(|| g.salience_sum / g.count as f64),
            salience_min: req.include_min_max.then_some(g.salience_min).flatten(),
            salience_max: req.include_min_max.then_some(g.salience_max).flatten(),
            access_count_sum: g.access_sum,
        })
        .collect();
    Ok(MemoryAggregateResponse {
        groups,
        source: AggregateSourceWire::Snapshot,
        as_of_unix_nanos: res.snapshot_taken_at_unix_nanos,
    })
}

fn kind_wire(b: u8) -> MemoryKindWire {
    match b {
        0 => MemoryKindWire::Episodic,
        1 => MemoryKindWire::Semantic,
        _ => MemoryKindWire::Consolidated,
    }
}

fn status_wire(b: u8) -> MemoryStatusWire {
    match b {
        agg::status::ACTIVE => MemoryStatusWire::Active,
        agg::status::TOMBSTONED => MemoryStatusWire::Tombstoned,
        _ => MemoryStatusWire::HardForgotten,
    }
}
