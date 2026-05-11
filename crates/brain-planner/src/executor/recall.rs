//! Executor for the `RECALL` cognitive operation.
//!
//! Async function (spec §08/08 §1) that orchestrates the
//! planner-produced `RecallPlan`:
//!
//! 1. Embed the cue (single call; cache hits stay sub-µs per
//!    `CachingDispatcher`).
//! 2. ANN search via [`SharedHnsw::search_active`] — tombstoned slots
//!    are filtered out as the pre-filter (spec §03 §6).
//! 3. Look up `MemoryMetadata` for each candidate from a single
//!    read txn.
//! 4. Apply the plan's post-filter rules.
//! 5. Sort by score, apply `confidence_min`, trim to `final_top`.
//! 6. Build `RecallResult`.
//!
//! No cooperative `.await` yields in this version — the per-shard
//! pipeline is synchronous from the planner's perspective and 6.7
//! will introduce yield points when Glommio's runtime arrives.

use brain_core::{ContextId, MemoryId, MemoryKind};
use brain_metadata::tables::memory::{MemoryMetadata, MEMORIES_TABLE};

use crate::plan::{FilterRule, FilterStage, RecallPlan, SortKey};

use super::context::ExecutorContext;
use super::error::ExecError;
use super::result::{RecallHit, RecallResult};

/// Execute a single-shard `RecallPlan`. Async to match spec §08/08 §1
/// even though the body has no `.await`; 6.7 wires runtime-specific
/// yields.
pub async fn execute_recall(
    plan: RecallPlan,
    ctx: &ExecutorContext,
) -> Result<RecallResult, ExecError> {
    // 1. Embed.
    let cue_vector = ctx.embedder.embed(&plan.embedding.text)?;

    // 2. ANN search. Single shard for v1 (orientation §4.7).
    let shard = plan
        .shards
        .first()
        .ok_or_else(|| ExecError::Internal("RecallPlan has no shards".into()))?;
    let ann = &shard.ann_search;
    let raw_hits: Vec<(MemoryId, f32)> =
        ctx.index
            .search_active(&cue_vector, ann.candidates_to_request, Some(ann.ef));

    // 3. Metadata lookup for each candidate (single read txn).
    let mut enriched: Vec<(RecallHit, f32)> = Vec::with_capacity(raw_hits.len());
    {
        let metadata_guard = ctx.metadata.lock();
        let txn = metadata_guard
            .read_txn()
            .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
        let table = txn
            .open_table(MEMORIES_TABLE)
            .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;

        for (memory_id, score) in raw_hits {
            let row = table
                .get(memory_id.to_be_bytes())
                .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
            let Some(access) = row else {
                // HNSW returned an id the metadata doesn't know about —
                // spec §08/10 says surface, don't swallow.
                return Err(ExecError::MemoryNotFound { memory_id });
            };
            let meta = access.value();
            let hit = build_hit(memory_id, score, &meta)?;
            enriched.push((hit, score));
        }
        // table + txn + guard drop here, releasing the mutex.
    }

    // 4. Apply post-filter rules.
    if shard.filter_apply.stage == FilterStage::PostFilter && !shard.filter_apply.rules.is_empty() {
        let rules = &shard.filter_apply.rules;
        enriched.retain(|(hit, _score)| rules.iter().all(|r| rule_matches(r, hit)));
    }

    // 5. Sort + confidence floor + trim.
    match plan.merge.sort_by {
        SortKey::Score => {
            enriched.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
        }
        SortKey::Salience => {
            enriched.sort_by(|a, b| {
                b.0.salience
                    .partial_cmp(&a.0.salience)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        SortKey::InsertedAt => {
            enriched.sort_by_key(|h| std::cmp::Reverse(h.0.created_at_unix_nanos));
        }
    }

    if let Some(min) = plan.merge.confidence_min {
        enriched.retain(|(_, score)| *score >= min);
    }

    enriched.truncate(plan.merge.final_top);

    Ok(RecallResult {
        hits: enriched.into_iter().map(|(h, _)| h).collect(),
    })
}

fn build_hit(
    memory_id: MemoryId,
    score: f32,
    meta: &MemoryMetadata,
) -> Result<RecallHit, ExecError> {
    let kind: MemoryKind = meta
        .kind()
        .map_err(|e| ExecError::Internal(format!("bad memory kind in metadata: {e:?}")))?;
    Ok(RecallHit {
        memory_id,
        score,
        kind,
        context_id: ContextId::from(meta.context_id),
        salience: meta.salience,
        created_at_unix_nanos: meta.created_at_unix_nanos,
        text: None,
    })
}

fn rule_matches(rule: &FilterRule, hit: &RecallHit) -> bool {
    match rule {
        FilterRule::KindIn(kinds) => kinds.contains(&hit.kind),
        FilterRule::ContextIn(ctx_ids) => ctx_ids.contains(&hit.context_id),
        FilterRule::SalienceFloor(threshold) => hit.salience >= *threshold,
        FilterRule::AgeBound {
            not_older_than_unix_nanos,
        } => hit.created_at_unix_nanos >= *not_older_than_unix_nanos,
        // ConfidenceFloor applies at merge time, not per-hit. If we
        // see one here treat it as always-pass (the merge step did the
        // filtering).
        FilterRule::ConfidenceFloor(_) => true,
    }
}
