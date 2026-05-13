//! Consolidation worker (sub-task 8.4). Spec §11/03.
//!
//! Identifies clusters of recent Episodic memories in the same
//! context, asks the [`Summarizer`] for a summary, and encodes the
//! result as a Consolidated memory with DERIVED_FROM edges back to
//! the sources. Source rows get their `consolidated_at_unix_nanos`
//! stamped so future cycles skip them.
//!
//! ## Idempotency
//!
//! Spec §8 wants encode + edges + source-stamps in one transaction.
//! v1 splits the source-stamp into a follow-up wtxn but achieves
//! restart-safety via a **deterministic `RequestId`** derived from
//! the sorted set of source ids. A partial-crash retry replays the
//! same encode (writer's idempotency cache returns the same memory
//! id) and re-stamps the sources (set-once + no-op if already set).
//! No duplicate Consolidated memories.
//!
//! ## Clustering
//!
//! Spec §4 calls for DBSCAN over vector cosine. v1's HNSW backend
//! doesn't expose `vector_for(memory_id)` (the arena lookup lands in
//! Phase 9), so the **worker can't run vector-based clustering yet**.
//!
//! The cycle therefore uses **window-based grouping**: within a
//! (context, recency_window) bucket, the worker treats the candidates
//! as a single cluster if at least `min_cluster_size` of them aren't
//! already consolidated. The proper similarity clustering is shipped
//! as a tested pure helper ([`cluster_by_similarity`]) for Phase 9 to
//! wire up once vectors become id-accessible. Documented v1 deviation
//! from spec §4.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use brain_core::{ContextId, EdgeKind, MemoryId, MemoryKind, RequestId};
use brain_embed::VECTOR_DIM;
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_planner::{EncodeOp, EncodeOpEdge};
use redb::ReadableTable;
use tracing::trace;
use uuid::Uuid;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::summarizer::{Summarizer, SummarizerError};
use crate::worker::Worker;

/// Spec §4 default — cosine similarity above which two memories are
/// considered "in the same cluster". Tunable.
pub const DEFAULT_SIMILARITY_THRESHOLD: f32 = 0.6;

/// Spec §4 default — minimum cluster size below which a group is
/// dropped (singletons / pairs aren't worth consolidating).
pub const DEFAULT_MIN_CLUSTER_SIZE: usize = 5;

/// Spec §4 default — only memories created within this window are
/// candidates ("temporally close" — 24 h).
pub const DEFAULT_RECENCY_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// Spec §11/02 §14 — Consolidated memories start with 0.7 salience.
/// 90-day half-life (spec §11/02 §1) gives them durable presence.
pub const DEFAULT_INITIAL_SALIENCE: f32 = 0.7;

// ---------------------------------------------------------------------------
// Cluster candidate + clustering algorithm.
// ---------------------------------------------------------------------------

/// Pure-helper input. Carries everything `cluster_by_similarity`
/// needs; Phase 9 will produce these from the arena.
#[derive(Clone, Debug)]
pub struct ClusterCandidate {
    pub memory_id: MemoryId,
    pub vector: [f32; VECTOR_DIM],
    pub created_at_unix_nanos: u64,
}

/// What the v1 worker actually scans (no vector lookup available).
#[derive(Clone, Debug)]
struct WindowCandidate {
    memory_id: MemoryId,
    created_at_unix_nanos: u64,
}

/// Compute cosine similarity. Returns 0.0 if either vector is the
/// zero vector (defensive — undefined cosine).
#[must_use]
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    debug_assert_eq!(a.len(), b.len());
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = f64::from(*x);
        let yf = f64::from(*y);
        dot += xf * yf;
        na += xf * xf;
        nb += yf * yf;
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    (dot / (na.sqrt() * nb.sqrt())) as f32
}

/// Single-linkage clustering on cosine similarity. Returns each
/// cluster as a `Vec<MemoryId>`; clusters of size < `min_cluster_size`
/// are dropped. Order within / across returned clusters is unspecified.
#[allow(clippy::needless_range_loop)] // i + j indexing is clearer here than enumerate
#[must_use]
pub fn cluster_by_similarity(
    candidates: &[ClusterCandidate],
    similarity_threshold: f32,
    min_cluster_size: usize,
) -> Vec<Vec<MemoryId>> {
    let n = candidates.len();
    if n < min_cluster_size {
        return Vec::new();
    }

    // Union-find over candidate indices.
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] == i {
            return i;
        }
        let root = find(parent, parent[i]);
        parent[i] = root;
        root
    }
    fn union(parent: &mut [usize], i: usize, j: usize) {
        let ri = find(parent, i);
        let rj = find(parent, j);
        if ri != rj {
            parent[ri] = rj;
        }
    }

    for i in 0..n {
        for j in (i + 1)..n {
            if cosine(&candidates[i].vector, &candidates[j].vector) >= similarity_threshold {
                union(&mut parent, i, j);
            }
        }
    }

    // Group indices by root.
    let mut groups: BTreeMap<usize, Vec<MemoryId>> = BTreeMap::new();
    for i in 0..n {
        let root = find(&mut parent, i);
        groups
            .entry(root)
            .or_default()
            .push(candidates[i].memory_id);
    }

    groups
        .into_values()
        .filter(|g| g.len() >= min_cluster_size)
        .collect()
}

// ---------------------------------------------------------------------------
// Deterministic request_id (idempotency).
// ---------------------------------------------------------------------------

/// Hash the sorted source ids to a stable 16-byte `RequestId`. Spec
/// §07/06 mandates that the same `RequestId` returns the same
/// `MemoryId`; this lets a partial-crash retry land on the same
/// Consolidated memory without duplication.
#[must_use]
pub fn deterministic_request_id(source_ids: &[MemoryId]) -> RequestId {
    let mut sorted: Vec<MemoryId> = source_ids.to_vec();
    sorted.sort();
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"brain/consolidation/v1");
    for id in &sorted {
        hasher.update(&id.to_be_bytes());
    }
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest.as_bytes()[..16]);
    RequestId(Uuid::from_bytes(bytes))
}

// ---------------------------------------------------------------------------
// ConsolidationWorker.
// ---------------------------------------------------------------------------

pub struct ConsolidationWorker {
    config: WorkerConfig,
    summarizer: Arc<dyn Summarizer>,
    similarity_threshold: f32,
    min_cluster_size: usize,
    recency_window: Duration,
    initial_salience: f32,
}

impl ConsolidationWorker {
    #[must_use]
    pub fn new(summarizer: Arc<dyn Summarizer>) -> Self {
        Self {
            config: WorkerConfig::defaults_for(WorkerKind::Consolidation),
            summarizer,
            similarity_threshold: DEFAULT_SIMILARITY_THRESHOLD,
            min_cluster_size: DEFAULT_MIN_CLUSTER_SIZE,
            recency_window: DEFAULT_RECENCY_WINDOW,
            initial_salience: DEFAULT_INITIAL_SALIENCE,
        }
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }
    #[must_use]
    pub fn with_similarity_threshold(mut self, t: f32) -> Self {
        self.similarity_threshold = t;
        self
    }
    #[must_use]
    pub fn with_min_cluster_size(mut self, n: usize) -> Self {
        self.min_cluster_size = n;
        self
    }
    #[must_use]
    pub fn with_recency_window(mut self, d: Duration) -> Self {
        self.recency_window = d;
        self
    }
    #[must_use]
    pub fn with_initial_salience(mut self, s: f32) -> Self {
        self.initial_salience = s;
        self
    }
}

impl Worker for ConsolidationWorker {
    fn name(&self) -> &'static str {
        WorkerKind::Consolidation.name()
    }
    fn kind(&self) -> WorkerKind {
        WorkerKind::Consolidation
    }
    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }
    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(do_consolidation_cycle(self, ctx))
    }
}

// ---------------------------------------------------------------------------
// Cycle body.
// ---------------------------------------------------------------------------

async fn do_consolidation_cycle(
    worker: &ConsolidationWorker,
    ctx: &WorkerContext,
) -> Result<usize, WorkerError> {
    let cfg = worker.config.clone();
    if cfg.batch_size == 0 {
        return Ok(0);
    }

    // Probe the summarizer once. If it's disabled, the worker is a
    // no-op (spec §6 / §16). We swallow Disabled silently.
    if let Err(SummarizerError::Disabled) = worker.summarizer.summarize(&[]).await {
        return Ok(0);
    }

    let now_nanos = now_unix_nanos();
    let recency_floor_nanos = now_nanos
        .saturating_sub(u64::try_from(worker.recency_window.as_nanos()).unwrap_or(u64::MAX));

    // 1. Collect candidate Episodics grouped by context. Single read
    //    txn; no .await inside the lock.
    let by_context = collect_candidates_by_context(ctx, recency_floor_nanos)?;
    let started = Instant::now();
    let mut consolidations = 0usize;

    for (context_id, candidates) in by_context {
        if started.elapsed() >= cfg.max_runtime {
            break;
        }
        if consolidations >= cfg.batch_size {
            break;
        }
        if ctx.is_shutdown() {
            break;
        }

        // v1 window-based grouping. Sort by recency and take the
        // oldest `min_cluster_size` ids as "the cluster" — older
        // memories are the ones most likely to benefit from
        // consolidation. (Spec §4's similarity clustering is the
        // Phase 9 upgrade; `cluster_by_similarity` ships as a pure
        // helper for that.)
        let mut sorted: Vec<WindowCandidate> = candidates;
        sorted.sort_by_key(|c| c.created_at_unix_nanos);
        if sorted.len() < worker.min_cluster_size {
            continue;
        }
        let cluster: Vec<MemoryId> = sorted
            .iter()
            .take(worker.min_cluster_size)
            .map(|c| c.memory_id)
            .collect();
        let clusters = vec![cluster];

        for cluster in clusters {
            if consolidations >= cfg.batch_size {
                break;
            }
            // Skip if any source is already consolidated (spec §11).
            if any_already_consolidated(ctx, &cluster)? {
                continue;
            }
            // Fetch source texts. Empty/missing rows → skip cluster.
            let Some(texts) = fetch_texts(ctx, &cluster)? else {
                continue;
            };
            // Summarize.
            let summary_texts: Vec<&str> = texts.iter().map(String::as_str).collect();
            let summary = match worker.summarizer.summarize(&summary_texts).await {
                Ok(s) => s,
                Err(SummarizerError::Disabled) => return Ok(consolidations),
                Err(SummarizerError::Failed(e)) => {
                    trace!(error = %e, "summarizer failed; skipping cluster");
                    continue;
                }
            };
            if summary.is_empty() {
                continue;
            }
            // Embed.
            let vector = ctx
                .ops
                .executor
                .embedder
                .embed(&summary)
                .map_err(|e| WorkerError::Ops(format!("embed: {e:?}")))?;
            // Encode.
            let request_id = deterministic_request_id(&cluster);
            let edges: Vec<EncodeOpEdge> = cluster
                .iter()
                .map(|id| EncodeOpEdge {
                    target: *id,
                    kind: EdgeKind::DerivedFrom,
                    weight: 1.0,
                })
                .collect();
            let op = EncodeOp {
                request_id,
                context_id,
                kind: MemoryKind::Consolidated,
                text: summary,
                vector,
                salience_initial: worker.initial_salience,
                fingerprint: ctx.ops.executor.embedder.fingerprint(),
                edges,
            };
            let ack = ctx
                .ops
                .executor
                .writer
                .submit_encode(op)
                .await
                .map_err(|e| WorkerError::Ops(format!("submit_encode: {e:?}")))?;
            // Stamp sources. Idempotent: set-once on consolidated_at.
            stamp_sources(ctx, &cluster, now_nanos)?;
            consolidations += 1;
            let _ = ack;
        }
    }

    trace!(
        consolidations,
        cycle_ms = started.elapsed().as_millis() as u64,
        "consolidation cycle"
    );
    Ok(consolidations)
}

// ---------------------------------------------------------------------------
// Helpers: read metadata, fetch texts, stamp.
// ---------------------------------------------------------------------------

fn collect_candidates_by_context(
    ctx: &WorkerContext,
    recency_floor_nanos: u64,
) -> Result<BTreeMap<ContextId, Vec<WindowCandidate>>, WorkerError> {
    let metadata = ctx.ops.executor.metadata.clone();
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("consolidation read_txn: {e:?}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;

    let mut by_context: BTreeMap<ContextId, Vec<WindowCandidate>> = BTreeMap::new();
    for entry in table
        .iter()
        .map_err(|e| WorkerError::Ops(format!("iter MEMORIES: {e:?}")))?
    {
        let (_, value) = entry.map_err(|e| WorkerError::Ops(format!("row: {e:?}")))?;
        let meta = value.value();
        if meta.created_at_unix_nanos < recency_floor_nanos {
            continue;
        }
        if meta.tombstoned_at_unix_nanos.is_some() {
            continue;
        }
        if meta.consolidated_at_unix_nanos.is_some() {
            continue; // already part of an earlier consolidation
        }
        let Ok(kind) = meta.kind() else { continue };
        if kind != MemoryKind::Episodic {
            continue;
        }
        by_context
            .entry(meta.context())
            .or_default()
            .push(WindowCandidate {
                memory_id: meta.memory_id(),
                created_at_unix_nanos: meta.created_at_unix_nanos,
            });
    }
    Ok(by_context)
}

fn any_already_consolidated(
    ctx: &WorkerContext,
    cluster: &[MemoryId],
) -> Result<bool, WorkerError> {
    let metadata = ctx.ops.executor.metadata.clone();
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("any-consol read_txn: {e:?}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
    for id in cluster {
        let row = table
            .get(id.to_be_bytes())
            .map_err(|e| WorkerError::Ops(format!("get: {e:?}")))?;
        if let Some(access) = row {
            if access.value().consolidated_at_unix_nanos.is_some() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// Read placeholder texts for the cluster's source memories. v1's
/// MemoryMetadata stores only `text_size`, not the actual text — so
/// we synthesize a stable string ("memory_<id>"). The text fed to
/// the summarizer is therefore deterministic but synthetic. Real
/// deployments inject a Summarizer that ignores the placeholder and
/// fetches text from the arena (Phase 9).
fn fetch_texts(
    ctx: &WorkerContext,
    cluster: &[MemoryId],
) -> Result<Option<Vec<String>>, WorkerError> {
    let metadata = ctx.ops.executor.metadata.clone();
    let db = metadata.lock();
    let rtxn = db
        .read_txn()
        .map_err(|e| WorkerError::Ops(format!("fetch_texts read_txn: {e:?}")))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| WorkerError::Ops(format!("open MEMORIES: {e:?}")))?;
    let mut texts = Vec::with_capacity(cluster.len());
    let mut seen: HashSet<MemoryId> = HashSet::with_capacity(cluster.len());
    for id in cluster {
        if !seen.insert(*id) {
            continue;
        }
        let row = table
            .get(id.to_be_bytes())
            .map_err(|e| WorkerError::Ops(format!("get: {e:?}")))?;
        let Some(access) = row else {
            return Ok(None);
        };
        // v1: no arena yet → use a synthetic placeholder. The text
        // is stable per memory_id so the request_id derived from the
        // cluster + summary is deterministic across retries.
        let _meta = access.value();
        texts.push(format!("memory_{}", id.raw()));
    }
    Ok(Some(texts))
}

fn stamp_sources(
    ctx: &WorkerContext,
    cluster: &[MemoryId],
    now_unix_nanos: u64,
) -> Result<(), WorkerError> {
    let metadata = ctx.ops.executor.metadata.clone();
    let mut db = metadata.lock();
    let wtxn = db
        .write_txn()
        .map_err(|e| WorkerError::Ops(format!("stamp write_txn: {e:?}")))?;
    {
        let mut table = wtxn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| WorkerError::Ops(format!("stamp open MEMORIES: {e:?}")))?;
        let unique: BTreeSet<MemoryId> = cluster.iter().copied().collect();
        for id in unique {
            let key = id.to_be_bytes();
            let prior = table
                .get(key)
                .map_err(|e| WorkerError::Ops(format!("stamp get: {e:?}")))?
                .map(|a| a.value());
            let Some(mut meta) = prior else { continue };
            if meta.consolidated_at_unix_nanos.is_some() {
                continue; // set-once idempotency
            }
            meta.consolidated_at_unix_nanos = Some(now_unix_nanos);
            table
                .insert(key, meta)
                .map_err(|e| WorkerError::Ops(format!("stamp insert: {e:?}")))?;
        }
    }
    wtxn.commit()
        .map_err(|e| WorkerError::Ops(format!("stamp commit: {e:?}")))?;
    Ok(())
}

fn now_unix_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
