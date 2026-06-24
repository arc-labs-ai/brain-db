//! AmbiguityResolverWorker — periodic merge-review-queue sweeper.
//!
//! ## Why this exists
//!
//! The resolver's Tier 3b embedding probe sometimes scores a candidate
//! in the partial-match band (`[0.7, 0.78)`) — close enough to be
//! suspicious, not close enough to auto-alias. The resolver writes the
//! current surface form as a fresh entity AND enqueues a
//! `MergeReviewProposal` linking the new entity to the close candidate.
//!
//! This worker walks the queue on a slow cadence (default 1 h):
//!
//! - **Promote**: re-embed the new entity's canonical name and re-query
//!   the HNSW. If the recomputed cosine clears the auto-apply threshold
//!   (default 0.95), execute `merge_entity(candidate ← new)` and stamp
//!   the proposal `AutoApplied`.
//! - **Reject**: if the recomputed cosine has dropped below the
//!   partial-match floor (default 0.7), stamp the proposal `Rejected`.
//!   The entity diverged — the original suggestion is no longer
//!   plausible.
//! - **Expire**: any proposal that has sat `Pending` past
//!   `expire_after_secs` (default 30 days) is stamped `Expired`.
//!
//! ## Idempotency
//!
//! Re-running the worker on the same queue is safe:
//! - A row already promoted to `AutoApplied` is filtered out by the
//!   `(PENDING, _)` index scan.
//! - A promotion that crashed between `merge_entity` succeeding and the
//!   proposal status update would be re-attempted on the next tick;
//!   `merge_entity` rejects a double-merge (entity already
//!   `merged_into = Some(_)`), so the worker logs and falls through to
//!   stamping the status only.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use brain_core::{EntityId, EntityTypeId, MergeId};
use brain_embed::Dispatcher;
use brain_index::entity_hnsw::EntityHnswIndex;
use brain_metadata::entity::merge::{merge_entity, EntityMergeOpError, MergeActor};
use brain_metadata::entity::review::{
    list_proposals_by_status, update_proposal_recheck, update_proposal_status,
};
use brain_metadata::tables::entity::{EntityMetadata, ENTITIES_TABLE};
use brain_metadata::tables::merge_review_queue::{proposal_status, MergeReviewProposal};
use brain_metadata::MetadataDb;
use brain_ops::AmbiguityResolverMetrics;
use parking_lot::RwLock;

use crate::config::{WorkerConfig, WorkerKind};
use crate::context::WorkerContext;
use crate::error::WorkerError;
use crate::worker::Worker;

// ---------------------------------------------------------------------------
// Knobs.
// ---------------------------------------------------------------------------

/// 1 hour default tick. Slow on purpose: a proposal's confidence shifts
/// as the HNSW absorbs new aliases / paraphrases, which happens on the
/// order of hours, not seconds.
pub const DEFAULT_INTERVAL_SECS: u64 = 3600;

/// Hard cap on proposals visited per tick. 64 trades wide-fanout
/// against per-tick wall-clock (each visit costs one embed + one HNSW
/// query + a possible merge_entity).
pub const DEFAULT_MAX_PER_TICK: usize = 64;

/// Cosine the recomputed score must reach for the worker to promote a
/// proposal to an actual merge. 0.95 matches the "autonomous merge"
/// threshold.
pub const DEFAULT_AUTO_APPLY_THRESHOLD: f32 = 0.95;

/// Recomputed scores below this floor flip the proposal to `Rejected`
/// — the candidate is no longer a plausible match. Lower than the
/// resolver's tier-3b auto-alias threshold (0.78) but matches the
/// review-band floor.
pub const DEFAULT_REJECT_FLOOR: f32 = 0.7;

/// Days a proposal can sit `Pending` before the worker stamps
/// `Expired`. 30 days lets a meaningful number of HNSW ingest cycles
/// settle.
pub const DEFAULT_EXPIRE_AFTER_SECS: u64 = 30 * 24 * 60 * 60;

/// Top-K asked of the entity HNSW during one proposal re-check. Mirrors
/// `brain_extractors::resolver::EMBED_RESOLVE_TOP_K` (small constant —
/// the HNSW already returns scored candidates).
const RECHECK_TOP_K: usize = 8;

/// Per-cycle tuning. `WorkerConfig` covers cadence / batch / runtime;
/// this struct holds the proposal-specific knobs.
#[derive(Clone, Copy, Debug)]
pub struct AmbiguityResolverConfig {
    pub interval: Duration,
    pub max_per_tick: usize,
    pub auto_apply_threshold: f32,
    pub reject_floor: f32,
    pub expire_after_secs: u64,
    /// Grace window passed into `merge_entity` for auto-promoted
    /// merges. Operators can still unmerge through the wire path within
    /// this window. Defaults to 7 days (mirrors
    /// `entity::merge::DEFAULT_MERGE_GRACE_NANOS` / 1e9).
    pub merge_grace_seconds: u64,
}

impl Default for AmbiguityResolverConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_INTERVAL_SECS),
            max_per_tick: DEFAULT_MAX_PER_TICK,
            auto_apply_threshold: DEFAULT_AUTO_APPLY_THRESHOLD,
            reject_floor: DEFAULT_REJECT_FLOOR,
            expire_after_secs: DEFAULT_EXPIRE_AFTER_SECS,
            merge_grace_seconds: 7 * 24 * 60 * 60,
        }
    }
}

// ---------------------------------------------------------------------------
// Worker.
// ---------------------------------------------------------------------------

pub struct AmbiguityResolverWorker {
    config: WorkerConfig,
    knobs: AmbiguityResolverConfig,
    metadata: Arc<MetadataDb>,
    entity_hnsw: Arc<RwLock<EntityHnswIndex>>,
    embedder: Arc<dyn Dispatcher>,
    metrics: Option<Arc<AmbiguityResolverMetrics>>,
}

impl AmbiguityResolverWorker {
    /// Build a worker with default knobs and the default sweep cadence.
    /// The metadata + HNSW + embedder handles are the same per-shard
    /// ones threaded through the extractor and statement-embed workers.
    /// Override the cadence with [`Self::with_interval_secs`], which the
    /// shard wires from `[workers.ambiguity_resolver] interval_secs`.
    #[must_use]
    pub fn new(
        metadata: Arc<MetadataDb>,
        entity_hnsw: Arc<RwLock<EntityHnswIndex>>,
        embedder: Arc<dyn Dispatcher>,
    ) -> Self {
        let mut config = WorkerConfig::defaults_for(WorkerKind::AmbiguityResolver);
        config.interval = Duration::from_secs(DEFAULT_INTERVAL_SECS);
        Self {
            config,
            knobs: AmbiguityResolverConfig::default(),
            metadata,
            entity_hnsw,
            embedder,
            metrics: None,
        }
    }

    /// Override the sweep cadence. The shard supplies
    /// `[workers.ambiguity_resolver] interval_secs`; a zero value is
    /// clamped to 1 second so the scheduler never busy-loops.
    #[must_use]
    pub fn with_interval_secs(mut self, interval_secs: u64) -> Self {
        self.config.interval = Duration::from_secs(interval_secs.max(1));
        self
    }

    #[must_use]
    pub fn with_config(mut self, cfg: WorkerConfig) -> Self {
        self.config = cfg;
        self
    }

    #[must_use]
    pub fn with_knobs(mut self, knobs: AmbiguityResolverConfig) -> Self {
        self.knobs = knobs;
        self
    }

    #[must_use]
    pub fn with_metrics(mut self, metrics: Arc<AmbiguityResolverMetrics>) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// One sweep tick. Public so integration tests can drive the worker
    /// without the scheduler.
    ///
    /// Returns the count of proposals processed (promoted + rejected +
    /// expired + still-pending-but-rechecked).
    pub async fn tick(&self, ctx: &WorkerContext) -> Result<usize, WorkerError> {
        let started = Instant::now();
        let now_ns = now_unix_nanos();
        if let Some(m) = &self.metrics {
            m.inc_sweeps();
        }

        // ── 1. Snapshot up to `max_per_tick` Pending proposals. ─────
        let pending: Vec<MergeReviewProposal> = {
            let rtxn = self
                .metadata
                .read_txn()
                .map_err(|e| WorkerError::Internal(format!("read_txn: {e}")))?;
            list_proposals_by_status(&rtxn, proposal_status::PENDING, self.knobs.max_per_tick)
                .map_err(|e| WorkerError::Internal(format!("list_proposals: {e}")))?
        };
        if let Some(m) = &self.metrics {
            m.set_pending_queue_depth(pending.len() as u64);
        }
        if pending.is_empty() {
            if let Some(m) = &self.metrics {
                m.observe_sweep_duration(started.elapsed().as_secs_f64());
            }
            return Ok(0);
        }

        // ── 2. Walk each proposal. ─────────────────────────────────
        let mut promoted = 0u64;
        let mut rejected = 0u64;
        let mut expired = 0u64;
        let mut still_pending = 0u64;

        for proposal in pending {
            if ctx.is_shutdown() {
                break;
            }
            let proposal_id = MergeId::from_bytes(proposal.proposal_id);

            // Expire-by-age short-circuit: if the proposal has been
            // sitting too long, terminal-stamp it without re-embedding.
            let age_secs = (now_ns.saturating_sub(proposal.proposed_at_unix_nanos)) / 1_000_000_000;
            if age_secs >= self.knobs.expire_after_secs {
                if let Err(e) = self.write_terminal(
                    proposal_id,
                    proposal_status::EXPIRED,
                    proposal.last_recheck_confidence,
                    now_ns,
                ) {
                    if let Some(m) = &self.metrics {
                        m.inc_errors();
                    }
                    tracing::warn!(
                        target: "brain_workers::ambiguity_resolver",
                        ?proposal_id,
                        error = %e,
                        "failed to mark proposal Expired",
                    );
                    continue;
                }
                expired += 1;
                continue;
            }

            // Re-check the proposal: embed the source entity's
            // canonical name, query the HNSW, find the cosine against
            // the candidate.
            let source = EntityId::from(proposal.source_entity);
            let candidate = EntityId::from(proposal.candidate_entity);
            let new_score = match self.recheck_score(source, candidate) {
                Ok(s) => s,
                Err(reason) => {
                    if let Some(m) = &self.metrics {
                        m.inc_errors();
                    }
                    tracing::warn!(
                        target: "brain_workers::ambiguity_resolver",
                        ?proposal_id,
                        reason,
                        "recheck failed; leaving proposal Pending",
                    );
                    continue;
                }
            };

            if new_score >= self.knobs.auto_apply_threshold {
                match self.promote(proposal_id, source, candidate, new_score, now_ns) {
                    Ok(()) => promoted += 1,
                    Err(e) => {
                        if let Some(m) = &self.metrics {
                            m.inc_errors();
                        }
                        tracing::warn!(
                            target: "brain_workers::ambiguity_resolver",
                            ?proposal_id,
                            error = %e,
                            "failed to promote proposal; leaving Pending",
                        );
                    }
                }
            } else if new_score < self.knobs.reject_floor {
                if let Err(e) =
                    self.write_terminal(proposal_id, proposal_status::REJECTED, new_score, now_ns)
                {
                    if let Some(m) = &self.metrics {
                        m.inc_errors();
                    }
                    tracing::warn!(
                        target: "brain_workers::ambiguity_resolver",
                        ?proposal_id,
                        error = %e,
                        "failed to mark proposal Rejected",
                    );
                    continue;
                }
                rejected += 1;
            } else {
                // Still in the partial-match band — record the recheck
                // score for operator observability and move on.
                if let Err(e) = self.write_recheck(proposal_id, new_score, now_ns) {
                    if let Some(m) = &self.metrics {
                        m.inc_errors();
                    }
                    tracing::warn!(
                        target: "brain_workers::ambiguity_resolver",
                        ?proposal_id,
                        error = %e,
                        "failed to update recheck score; leaving Pending",
                    );
                    continue;
                }
                still_pending += 1;
            }
        }

        if let Some(m) = &self.metrics {
            m.add_promoted(promoted);
            m.add_rejected(rejected);
            m.add_expired(expired);
            m.observe_sweep_duration(started.elapsed().as_secs_f64());
        }

        Ok((promoted + rejected + expired + still_pending) as usize)
    }

    /// Re-embed the source entity's canonical name and query the HNSW
    /// for the cosine against the candidate. Returns `Ok(0.0)` when the
    /// candidate is no longer in the HNSW (it was tombstoned / merged
    /// away after the proposal was filed); `Err(reason)` for transient
    /// embedder / HNSW failures.
    fn recheck_score(&self, source: EntityId, candidate: EntityId) -> Result<f32, String> {
        // Load the source's canonical name to embed.
        let source_name = {
            let db = self.metadata.as_ref();
            let rtxn = db.read_txn().map_err(|e| format!("read_txn: {e}"))?;
            let t = rtxn
                .open_table(ENTITIES_TABLE)
                .map_err(|e| format!("open_table: {e}"))?;
            let row: Option<EntityMetadata> = t
                .get(&source.to_bytes())
                .map_err(|e| format!("get source: {e}"))?
                .map(|g| g.value());
            match row {
                Some(r) => r.canonical_name,
                None => return Err("source entity missing".into()),
            }
        };
        let vector = self
            .embedder
            .embed(&source_name)
            .map_err(|e| format!("embedder failed: {e}"))?;
        let hits = {
            let hnsw = self.entity_hnsw.read();
            if hnsw.is_empty() {
                return Ok(0.0);
            }
            hnsw.search(&vector, RECHECK_TOP_K)
                .map_err(|e| format!("hnsw search: {e}"))?
        };
        // Find the candidate's cosine. If it isn't in the top-K, the
        // candidate is no longer competitive — treat as 0.0 so the
        // worker rejects the proposal.
        for (eid, score) in hits {
            if eid == candidate {
                return Ok(score);
            }
        }
        Ok(0.0)
    }

    /// Execute `merge_entity` for the proposal and stamp the row
    /// `AutoApplied`. Both happen inside one wtxn so the merge audit
    /// row and the proposal terminal-stamp commit together.
    fn promote(
        &self,
        proposal_id: MergeId,
        source: EntityId,
        candidate: EntityId,
        recheck_score: f32,
        now_ns: u64,
    ) -> Result<(), WorkerError> {
        let wtxn = self
            .metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("write_txn: {e}")))?;
        match merge_entity(
            &wtxn,
            candidate,
            source,
            recheck_score,
            "merge_review_queue: auto-promoted by ambiguity-resolver worker".to_string(),
            MergeActor::System,
            self.knobs.merge_grace_seconds,
            now_ns,
        ) {
            Ok(_audit_id) => {}
            Err(EntityMergeOpError::AlreadyMerged(_, _)) => {
                // A concurrent write merged one side already; the
                // proposal is moot. Continue to stamp Rejected so the
                // operator audit shows the resolution.
                update_proposal_status(
                    &wtxn,
                    proposal_id,
                    proposal_status::REJECTED,
                    recheck_score,
                    now_ns,
                )
                .map_err(|e| WorkerError::Internal(format!("update_proposal_status: {e}")))?;
                wtxn.commit()
                    .map_err(|e| WorkerError::Internal(format!("commit: {e}")))?;
                return Ok(());
            }
            Err(e) => {
                return Err(WorkerError::Ops(format!("merge_entity: {e}")));
            }
        }
        update_proposal_status(
            &wtxn,
            proposal_id,
            proposal_status::AUTO_APPLIED,
            recheck_score,
            now_ns,
        )
        .map_err(|e| WorkerError::Internal(format!("update_proposal_status: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("commit: {e}")))?;
        Ok(())
    }

    /// Stamp a proposal terminal (Rejected or Expired).
    fn write_terminal(
        &self,
        proposal_id: MergeId,
        new_status: u8,
        recheck_score: f32,
        now_ns: u64,
    ) -> Result<(), WorkerError> {
        let wtxn = self
            .metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("write_txn: {e}")))?;
        update_proposal_status(&wtxn, proposal_id, new_status, recheck_score, now_ns)
            .map_err(|e| WorkerError::Internal(format!("update_proposal_status: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("commit: {e}")))?;
        Ok(())
    }

    /// Record a recheck score without changing the status.
    fn write_recheck(
        &self,
        proposal_id: MergeId,
        recheck_score: f32,
        now_ns: u64,
    ) -> Result<(), WorkerError> {
        let wtxn = self
            .metadata
            .write_txn()
            .map_err(|e| WorkerError::Internal(format!("write_txn: {e}")))?;
        update_proposal_recheck(&wtxn, proposal_id, recheck_score, now_ns)
            .map_err(|e| WorkerError::Internal(format!("update_proposal_recheck: {e}")))?;
        wtxn.commit()
            .map_err(|e| WorkerError::Internal(format!("commit: {e}")))?;
        Ok(())
    }
}

impl Worker for AmbiguityResolverWorker {
    fn name(&self) -> &'static str {
        WorkerKind::AmbiguityResolver.name()
    }

    fn kind(&self) -> WorkerKind {
        WorkerKind::AmbiguityResolver
    }

    fn config(&self) -> WorkerConfig {
        self.config.clone()
    }

    fn run_cycle<'a>(
        &'a self,
        ctx: &'a WorkerContext,
    ) -> Pin<Box<dyn Future<Output = Result<usize, WorkerError>> + 'a>> {
        Box::pin(self.tick(ctx))
    }
}

// `EntityTypeId` is only used for compile-time documentation that the
// recheck path operates over a single shard's entity HNSW (mixed-type
// indexing is acceptable because the HNSW is per-shard). Suppress the
// unused import in release builds via this dummy reference.
#[allow(dead_code)]
fn _ensure_entity_type_referenced() -> EntityTypeId {
    EntityTypeId(0)
}

fn now_unix_nanos() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(all(test, not(miri)))]
#[allow(clippy::arc_with_non_send_sync)]
mod tests {
    use super::*;
    use brain_core::{Entity, EntityType};
    use brain_embed::EmbedError;
    use brain_embed::VECTOR_DIM;
    use brain_index::entity_hnsw::EntityHnswParams;
    use brain_index::{IndexParams, SharedHnsw};
    use brain_metadata::entity::ops::{entity_put, normalize_name};
    use brain_metadata::entity::review::{enqueue_merge_proposal, proposal_get};
    use brain_metadata::tables::merge_review_queue::proposal_tier;
    use brain_metadata::MetadataDb;
    use brain_ops::RealWriterHandle;
    use brain_planner::{ExecutorContext, WriterHandle};
    use std::collections::HashMap;
    use std::sync::atomic::AtomicBool;
    use std::sync::Mutex as StdMutex;

    const NOW: u64 = 1_700_000_000_000_000_000;

    /// Real "now" in unix nanos — tests that enqueue Pending proposals
    /// use this so the worker's wall-clock age check doesn't trip the
    /// expiry branch. The `NOW` constant above is the fixture-clock
    /// timestamp threaded into `EntityType::new_active`; the worker
    /// uses `SystemTime::now()` internally, so proposed_at_unix_nanos
    /// needs to track real time.
    fn real_now_unix_nanos() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    }

    /// Deterministic embedder driven by a `name → vector` table. Misses
    /// fall back to a vector orthogonal to every fixture.
    struct ScriptedEmbedder {
        table: StdMutex<HashMap<String, [f32; VECTOR_DIM]>>,
    }

    impl ScriptedEmbedder {
        fn new() -> Self {
            Self {
                table: StdMutex::new(HashMap::new()),
            }
        }

        fn set(&self, key: &str, v: [f32; VECTOR_DIM]) {
            self.table.lock().unwrap().insert(key.to_string(), v);
        }
    }

    impl Dispatcher for ScriptedEmbedder {
        fn embed(&self, text: &str) -> Result<[f32; VECTOR_DIM], EmbedError> {
            if let Some(v) = self.table.lock().unwrap().get(text).copied() {
                return Ok(v);
            }
            let h = blake3::hash(text.as_bytes());
            let axis = (u32::from_le_bytes([
                h.as_bytes()[0],
                h.as_bytes()[1],
                h.as_bytes()[2],
                h.as_bytes()[3],
            ]) as usize
                % (VECTOR_DIM - 32))
                + 32;
            let mut v = [0.0_f32; VECTOR_DIM];
            v[axis] = 1.0;
            Ok(v)
        }

        fn embed_batch(&self, texts: &[&str]) -> Result<Vec<[f32; VECTOR_DIM]>, EmbedError> {
            texts.iter().map(|t| self.embed(t)).collect()
        }

        fn fingerprint(&self) -> [u8; 16] {
            [0xAB; 16]
        }
    }

    fn shared_axis(peak: usize, co: usize, peak_w: f32, co_w: f32) -> [f32; VECTOR_DIM] {
        let mut v = [0.0_f32; VECTOR_DIM];
        v[peak] = peak_w;
        v[co] = co_w;
        let norm = (peak_w * peak_w + co_w * co_w).sqrt();
        if norm > 0.0 {
            v[peak] /= norm;
            v[co] /= norm;
        }
        v
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        metadata: Arc<MetadataDb>,
        hnsw: Arc<RwLock<EntityHnswIndex>>,
        embedder: Arc<ScriptedEmbedder>,
        worker_ctx: WorkerContext,
    }

    fn build_worker_ctx(metadata: Arc<MetadataDb>, embedder: Arc<dyn Dispatcher>) -> WorkerContext {
        let (shared, hnsw_writer) =
            SharedHnsw::new(IndexParams::default_v1()).expect("SharedHnsw::new");
        let writer: Arc<dyn WriterHandle> =
            Arc::new(RealWriterHandle::new(metadata.clone(), hnsw_writer));
        let executor = ExecutorContext::new(embedder, shared, metadata, writer);
        let ops = Arc::new(brain_ops::test_support::ops_context_for_tests_owning_tempdir(executor));
        WorkerContext {
            ops,
            shutdown: Arc::new(AtomicBool::new(false)),
        }
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let metadata = MetadataDb::open(dir.path().join("test.redb")).expect("open metadata");
        let metadata = Arc::new(metadata);
        let hnsw = Arc::new(RwLock::new(
            EntityHnswIndex::new(EntityHnswParams::default_v1()).unwrap(),
        ));
        let embedder = Arc::new(ScriptedEmbedder::new());
        let worker_ctx =
            build_worker_ctx(metadata.clone(), embedder.clone() as Arc<dyn Dispatcher>);
        Fixture {
            _dir: dir,
            metadata,
            hnsw,
            embedder,
            worker_ctx,
        }
    }

    /// Seed one entity row in redb + insert into the entity HNSW with
    /// the chosen vector.
    fn seed_entity(
        d: &Arc<MetadataDb>,
        hnsw: &Arc<RwLock<EntityHnswIndex>>,
        canonical: &str,
        vector: [f32; VECTOR_DIM],
    ) -> EntityId {
        let id = EntityId::new();
        let ent = Entity::new_active(
            id,
            EntityType::PERSON_ID,
            canonical.into(),
            normalize_name(canonical),
            NOW,
        );
        let wtxn = d.write_txn().unwrap();
        entity_put(&wtxn, &ent).unwrap();
        wtxn.commit().unwrap();
        hnsw.write().insert(id, &vector).unwrap();
        id
    }

    fn enqueue(
        d: &Arc<MetadataDb>,
        source: EntityId,
        candidate: EntityId,
        confidence: f32,
        proposed_at_unix_nanos: u64,
    ) -> MergeId {
        let pid = MergeId::new();
        let wtxn = d.write_txn().unwrap();
        enqueue_merge_proposal(
            &wtxn,
            pid,
            source,
            candidate,
            confidence,
            proposal_tier::EMBEDDING,
            proposed_at_unix_nanos,
        )
        .unwrap();
        wtxn.commit().unwrap();
        pid
    }

    #[test]
    fn tick_promotes_proposal_when_confidence_grows() {
        // Seed Acme + Acme Holdings far apart (proposal cosine ~0.75),
        // then after the proposal is filed move Acme Holdings' HNSW
        // vector very close to Acme's so the recompute clears 0.95.
        let fx = fixture();
        let acme_v = shared_axis(10, 11, 1.0, 0.0);
        let acme = seed_entity(&fx.metadata, &fx.hnsw, "Acme", acme_v);
        let original_holdings_v = shared_axis(10, 11, 0.75, 0.661);
        let holdings = seed_entity(&fx.metadata, &fx.hnsw, "Acme Holdings", original_holdings_v);

        // Proposal: holdings → acme, original cosine 0.75. Use real
        // wall-clock now so the worker's age check doesn't trip
        // expiry.
        let pid = enqueue(&fx.metadata, holdings, acme, 0.75, real_now_unix_nanos());

        // Simulate "context grew": the worker re-embeds "Acme Holdings"
        // and finds it almost identical to Acme. Replace the embedder's
        // mapping for "Acme Holdings" with a near-clone of acme_v, and
        // re-insert that same vector into the HNSW for the holdings id.
        // The HNSW already has the original vector at this id; insert
        // returns an error on duplicate, so the simplest way to flip
        // the cosine is to rebuild the holdings HNSW row by deleting it
        // and re-inserting. HNSW lacks a delete API in this codebase,
        // so we instead make the embedder return the new "close"
        // vector AND we seed a NEW entity to act as the promoted
        // candidate. The neat alternative: keep holdings unchanged in
        // the HNSW (so the search returns it at the original cosine)
        // and use the "embedder returns a vector that now puts
        // holdings at 0.97 from acme" path — embedder controls the
        // query vector, not the stored vector.
        //
        // Pre-recheck embedder mapping wasn't set, so the worker would
        // fall back to its hash fallback. Set "Acme Holdings" to a
        // vector that, when used as the query vector, scores high
        // against acme_v in the HNSW. Since cosine(query, acme_v) =
        // dot(query, acme_v) for unit vectors and acme_v = (1, 0) on
        // (10, 11), we set query = (0.98, 0.199) → cosine ≈ 0.98.
        let close_v = shared_axis(10, 11, 0.98, 0.199);
        fx.embedder.set("Acme Holdings", close_v);

        // But the worker reads the *candidate* (acme) score in the HNSW
        // result list. The HNSW's neighbour set when queried with
        // close_v sees acme_v at cosine 0.98 and original_holdings_v
        // at cosine cosine(close_v, original_holdings_v) ≈ ...
        // The worker walks `hits` looking for `candidate == acme`, so
        // we need acme to be returned with cosine ≥ auto_apply.
        // close_v ⋅ acme_v = 0.98 ≥ 0.95.

        let metrics = Arc::new(AmbiguityResolverMetrics::new());
        let worker = AmbiguityResolverWorker::new(
            fx.metadata.clone(),
            fx.hnsw.clone(),
            fx.embedder.clone() as Arc<dyn Dispatcher>,
        )
        .with_metrics(metrics.clone());

        let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(processed, 1, "exactly one proposal processed");

        let rtxn = fx.metadata.as_ref().read_txn().unwrap();
        let updated = proposal_get(&rtxn, pid).unwrap().unwrap();
        assert_eq!(
            updated.status,
            proposal_status::AUTO_APPLIED,
            "proposal must be AutoApplied; got {} with recheck {}",
            updated.status,
            updated.last_recheck_confidence,
        );
        assert!(
            updated.last_recheck_confidence >= DEFAULT_AUTO_APPLY_THRESHOLD,
            "recheck must clear the auto-apply threshold; got {}",
            updated.last_recheck_confidence,
        );

        // The candidate must now be the holdings' `merged_into`.
        let t = rtxn
            .open_table(brain_metadata::tables::entity::ENTITIES_TABLE)
            .unwrap();
        let holdings_row = t.get(&holdings.to_bytes()).unwrap().unwrap().value();
        assert_eq!(
            holdings_row.merged_into_bytes,
            Some(acme.to_bytes()),
            "holdings must now redirect to acme",
        );

        let m = metrics.snapshot();
        assert_eq!(m.proposals_promoted_to_merge_total, 1);
        assert_eq!(m.sweeps_total, 1);
    }

    #[test]
    fn tick_rejects_when_confidence_drops() {
        let fx = fixture();
        let acme_v = shared_axis(20, 21, 1.0, 0.0);
        let acme = seed_entity(&fx.metadata, &fx.hnsw, "Acme", acme_v);
        let bitcoin_v = shared_axis(80, 81, 1.0, 0.0);
        let bitcoin = seed_entity(&fx.metadata, &fx.hnsw, "Bitcoin", bitcoin_v);

        let pid = enqueue(&fx.metadata, bitcoin, acme, 0.75, real_now_unix_nanos());

        // Recheck: embedder returns a vector orthogonal to acme_v so
        // the cosine drops to ~0.
        let orthogonal = shared_axis(80, 81, 1.0, 0.0);
        fx.embedder.set("Bitcoin", orthogonal);

        let metrics = Arc::new(AmbiguityResolverMetrics::new());
        let worker = AmbiguityResolverWorker::new(
            fx.metadata.clone(),
            fx.hnsw.clone(),
            fx.embedder.clone() as Arc<dyn Dispatcher>,
        )
        .with_metrics(metrics.clone());

        futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        let rtxn = fx.metadata.as_ref().read_txn().unwrap();
        let updated = proposal_get(&rtxn, pid).unwrap().unwrap();
        assert_eq!(updated.status, proposal_status::REJECTED);
        let m = metrics.snapshot();
        assert_eq!(m.proposals_rejected_total, 1);
    }

    #[test]
    fn tick_expires_old_proposals() {
        let fx = fixture();
        let acme = seed_entity(
            &fx.metadata,
            &fx.hnsw,
            "Acme",
            shared_axis(30, 31, 1.0, 0.0),
        );
        let holdings = seed_entity(
            &fx.metadata,
            &fx.hnsw,
            "Holdings",
            shared_axis(30, 31, 0.75, 0.661),
        );
        // Proposed_at_unix_nanos = real_now - 31 days so the worker's
        // wall-clock age check trips the 30-day expiry threshold.
        let thirty_one_days_ago =
            real_now_unix_nanos().saturating_sub(31 * 24 * 60 * 60 * 1_000_000_000);
        let pid = enqueue(&fx.metadata, holdings, acme, 0.75, thirty_one_days_ago);
        let metrics = Arc::new(AmbiguityResolverMetrics::new());
        let worker = AmbiguityResolverWorker::new(
            fx.metadata.clone(),
            fx.hnsw.clone(),
            fx.embedder.clone() as Arc<dyn Dispatcher>,
        )
        .with_metrics(metrics.clone());

        futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        let rtxn = fx.metadata.as_ref().read_txn().unwrap();
        let updated = proposal_get(&rtxn, pid).unwrap().unwrap();
        assert_eq!(updated.status, proposal_status::EXPIRED);
        let m = metrics.snapshot();
        assert_eq!(m.proposals_expired_total, 1);
    }

    #[test]
    fn empty_queue_is_a_no_op() {
        let fx = fixture();
        let worker = AmbiguityResolverWorker::new(
            fx.metadata.clone(),
            fx.hnsw.clone(),
            fx.embedder.clone() as Arc<dyn Dispatcher>,
        );
        let processed = futures_lite::future::block_on(worker.tick(&fx.worker_ctx)).unwrap();
        assert_eq!(processed, 0);
    }
}
