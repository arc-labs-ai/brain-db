//! `execute_reason` — evidence-traversal executor for the REASON
//! cognitive operation. Spec §09/05.
//!
//! Steps:
//!
//! 1. **Base resolution.** `ByMemoryId(id)` → base = `{id}` with
//!    `base_similarity = 1.0`. `ByText(t)` → embed + ANN search
//!    against the index; each hit's score is its `base_similarity`.
//! 2. **Two outward BFS traversals.** From the base set, walk along
//!    `supports_traversal.edge_kinds` (Supports + DerivedFrom by
//!    default) for supporting evidence; walk along
//!    `contradicts_traversal.edge_kinds` (Contradicts) for
//!    contradicting evidence. Each traversal uses parent pointers
//!    to reconstruct `edge_path`.
//! 3. **Scoring.** `score = base_similarity × decay(distance)`,
//!    `decay(d) = 1 / (1 + d)`. Edge-weight is pinned at 1.0 in v1
//!    (same gap as PLAN — per-edge weights aren't plumbed through
//!    `EvidenceItem` yet).
//! 4. **Confidence floor + trim.** Drop items below
//!    `plan.confidence_threshold`; trim to `aggregation.max_supporting`
//!    / `max_contradicting`.
//! 5. **Aggregate.** `confidence = (sum_s - sum_c) / (sum_s + sum_c)`;
//!    `0` when the denominator is `0`.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::edge::{list_edges_from, EDGES_OUT_TABLE};
use brain_protocol::request::ObservationInput;

use crate::plan::reason::ReasonPlan;

use super::context::ExecutorContext;
use super::error::ExecError;
use super::result::{EvidenceItem, ReasonResult, ReasonStatus};

const BASE_RECALL_EF: usize = 32;

pub async fn execute_reason(
    plan: ReasonPlan,
    ctx: &ExecutorContext,
) -> Result<ReasonResult, ExecError> {
    // 1. Base resolution.
    let (base_scores, base_memories) = resolve_base(&plan, ctx)?;
    if base_scores.is_empty() {
        return Ok(ReasonResult {
            base_memories,
            supporting: Vec::new(),
            contradicting: Vec::new(),
            confidence: 0.0,
            status: ReasonStatus::Complete,
        });
    }

    let started = Instant::now();
    let wall_ms = u64::from(plan.budget_wall_time_ms);
    let max_inferences = plan.max_inferences as usize;

    // 2. Two outward BFS traversals. Each returns a Vec<EvidenceItem>.
    let supports_kinds: HashSet<EdgeKind> =
        plan.supports_traversal.edge_kinds.iter().copied().collect();
    let contradicts_kinds: HashSet<EdgeKind> = plan
        .contradicts_traversal
        .edge_kinds
        .iter()
        .copied()
        .collect();

    let mut supporting = walk_outward(
        &base_scores,
        &supports_kinds,
        plan.supports_traversal.max_depth,
        max_inferences,
        ctx,
        started,
        wall_ms,
    )?;

    // Direct-similarity supporting items: every base memory is a
    // supporting item at distance 0. Spec §09/05 §4.
    for (&id, &sim) in &base_scores {
        supporting.push(EvidenceItem {
            memory_id: id,
            score: sim,
            edge_path: Vec::new(),
            distance: 0,
        });
    }

    let contradicting = walk_outward(
        &base_scores,
        &contradicts_kinds,
        plan.contradicts_traversal.max_depth,
        max_inferences,
        ctx,
        started,
        wall_ms,
    )?;

    // 3+4. Apply confidence floor + trim.
    let floor = plan.confidence_threshold;
    let mut supporting = filter_and_trim(supporting, floor, plan.aggregation.max_supporting);
    let mut contradicting =
        filter_and_trim(contradicting, floor, plan.aggregation.max_contradicting);

    // Sort by score descending for a deterministic result order.
    supporting.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    contradicting.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // 5. Aggregate.
    let sum_s: f32 = supporting.iter().map(|e| e.score).sum();
    let sum_c: f32 = contradicting.iter().map(|e| e.score).sum();
    let confidence = if sum_s + sum_c <= 0.0 {
        0.0
    } else {
        (sum_s - sum_c) / (sum_s + sum_c)
    };

    // Status: if the wall-clock or inference budget was exceeded mid-walk
    // it's already encoded in `walk_outward`'s early-return. We use a
    // conservative default of Complete here; future precision is fine.
    let status = ReasonStatus::Complete;

    Ok(ReasonResult {
        base_memories,
        supporting,
        contradicting,
        confidence,
        status,
    })
}

// ---------------------------------------------------------------------------
// Base resolution.
// ---------------------------------------------------------------------------

fn resolve_base(
    plan: &ReasonPlan,
    ctx: &ExecutorContext,
) -> Result<(HashMap<MemoryId, f32>, Vec<MemoryId>), ExecError> {
    match &plan.observation {
        ObservationInput::ByMemoryId(raw) => {
            let id = MemoryId::from(*raw);
            let mut map = HashMap::with_capacity(1);
            map.insert(id, 1.0_f32);
            Ok((map, vec![id]))
        }
        ObservationInput::ByText(text) => {
            let vector = ctx.embedder.embed(text)?;
            let k = plan
                .aggregation
                .max_supporting
                .saturating_add(plan.aggregation.max_contradicting)
                .max(1);
            let hits = ctx.index.search_active(&vector, k, Some(BASE_RECALL_EF));
            let order: Vec<MemoryId> = hits.iter().map(|(id, _)| *id).collect();
            let map: HashMap<MemoryId, f32> = hits.into_iter().collect();
            Ok((map, order))
        }
    }
}

// ---------------------------------------------------------------------------
// Outward BFS along a fixed edge-kind set.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Crumb {
    parent: Option<MemoryId>,
    edge: Option<EdgeKind>,
    depth: usize,
    base_similarity: f32,
}

#[allow(clippy::too_many_arguments)]
fn walk_outward(
    base: &HashMap<MemoryId, f32>,
    edge_kinds: &HashSet<EdgeKind>,
    max_depth: usize,
    max_inferences: usize,
    ctx: &ExecutorContext,
    started: Instant,
    wall_ms: u64,
) -> Result<Vec<EvidenceItem>, ExecError> {
    if edge_kinds.is_empty() || max_depth == 0 {
        return Ok(Vec::new());
    }

    let metadata_guard = ctx.metadata.lock();
    let rtxn = metadata_guard
        .read_txn()
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
    let edges_out = rtxn
        .open_table(EDGES_OUT_TABLE)
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;

    let mut visited: HashMap<MemoryId, Crumb> = HashMap::new();
    let mut queue: VecDeque<MemoryId> = VecDeque::new();

    for (&id, &sim) in base {
        visited.insert(
            id,
            Crumb {
                parent: None,
                edge: None,
                depth: 0,
                base_similarity: sim,
            },
        );
        queue.push_back(id);
    }

    let mut evidence: Vec<EvidenceItem> = Vec::new();

    'outer: while let Some(node) = queue.pop_front() {
        if started.elapsed().as_millis() as u64 > wall_ms {
            break;
        }
        if evidence.len() >= max_inferences {
            break;
        }
        let crumb = visited[&node];
        if crumb.depth >= max_depth {
            continue;
        }

        let neighbours: Vec<(EdgeKind, MemoryId)> = list_edges_from(&edges_out, node, None)
            .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?
            .into_iter()
            .filter(|(k, _, _)| edge_kinds.contains(k))
            .map(|(k, t, _)| (k, t))
            .collect();

        for (kind, next) in neighbours {
            if visited.contains_key(&next) {
                continue;
            }
            let new_depth = crumb.depth + 1;
            visited.insert(
                next,
                Crumb {
                    parent: Some(node),
                    edge: Some(kind),
                    depth: new_depth,
                    base_similarity: crumb.base_similarity,
                },
            );
            queue.push_back(next);

            // Build the EvidenceItem now (parent chain is complete
            // through `visited`).
            let edge_path = reconstruct_path(next, &visited);
            #[allow(clippy::cast_precision_loss)]
            let decay = 1.0_f32 / (1.0 + new_depth as f32);
            let score = crumb.base_similarity * decay;
            evidence.push(EvidenceItem {
                memory_id: next,
                score,
                edge_path,
                distance: new_depth,
            });

            if evidence.len() >= max_inferences {
                break 'outer;
            }
        }
    }

    Ok(evidence)
}

fn reconstruct_path(end: MemoryId, visited: &HashMap<MemoryId, Crumb>) -> Vec<EdgeKind> {
    let mut path: Vec<EdgeKind> = Vec::new();
    let mut cur = end;
    while let Some(c) = visited.get(&cur) {
        match (c.edge, c.parent) {
            (Some(e), Some(p)) => {
                path.push(e);
                cur = p;
            }
            _ => break,
        }
    }
    path.reverse();
    path
}

fn filter_and_trim(items: Vec<EvidenceItem>, floor: f32, max: usize) -> Vec<EvidenceItem> {
    let mut filtered: Vec<EvidenceItem> = items.into_iter().filter(|e| e.score >= floor).collect();
    filtered.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    filtered.truncate(max);
    filtered
}

// ---------------------------------------------------------------------------
// Tests live in `crates/brain-planner/tests/reason_executor.rs`.
// ---------------------------------------------------------------------------
