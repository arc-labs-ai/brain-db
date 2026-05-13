//! `execute_path` — bidirectional-BFS executor for the PLAN
//! cognitive operation. Spec §09/04.
//!
//! Steps:
//!
//! 1. **Endpoint resolution.** `PlanState::ByMemoryId` is used
//!    directly. `PlanState::ByText` runs a small ANN search against
//!    the index to anchor the endpoint at the K nearest memories.
//! 2. **Bidirectional BFS.** Alternately expand the smaller frontier
//!    by one hop, scanning `edges_out` (forward) or `edges_in`
//!    (backward) filtered to the plan's `edge_kinds`. Stop on
//!    intersection or budget exhaustion.
//! 3. **Path scoring.** Per spec §09/04 §10:
//!    `score = length × edge_weight × salience` (geometric mean for
//!    edge-weight and salience).
//! 4. **Truncate** to `scoring.top_n` and return.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use brain_core::{EdgeKind, MemoryId};
use brain_metadata::tables::edge::{
    list_edges_from, list_edges_to, EDGES_IN_TABLE, EDGES_OUT_TABLE,
};
use brain_metadata::tables::memory::MEMORIES_TABLE;
use brain_protocol::request::PlanState;

use crate::plan::path::PathPlan;

use super::context::ExecutorContext;
use super::error::ExecError;
use super::result::{Path, PathResult, PlanStatus};

const ENDPOINT_RECALL_K: usize = 5;
const ENDPOINT_RECALL_EF: usize = 32;

pub async fn execute_path(plan: PathPlan, ctx: &ExecutorContext) -> Result<PathResult, ExecError> {
    // 1. Resolve endpoints. ByMemoryId is direct; ByText runs a
    //    small ANN search; ByVector isn't wired yet.
    let starts = resolve_endpoint(&plan.start, ctx)?;
    let goals = resolve_endpoint(&plan.goal, ctx)?;

    if starts.is_empty() || goals.is_empty() {
        return Ok(PathResult {
            paths: Vec::new(),
            status: PlanStatus::NoPathFound,
        });
    }

    // 2. Bi-BFS along the configured edge kinds.
    let edge_kinds: HashSet<EdgeKind> = plan.traversal.edge_kinds.iter().copied().collect();
    let bfs = run_bidirectional_bfs(
        &starts,
        &goals,
        plan.traversal.max_depth,
        &edge_kinds,
        plan.budget.max_branches_explored as usize,
        plan.budget.max_wall_time_ms as u64,
        plan.traversal.max_paths,
        ctx,
    )?;

    if bfs.paths.is_empty() {
        return Ok(PathResult {
            paths: Vec::new(),
            status: bfs.status,
        });
    }

    // 3. Hydrate node metadata (salience + text for the wire frame).
    let mut paths = hydrate_paths(bfs.paths, ctx)?;

    // 4. Score + sort + truncate.
    for p in &mut paths {
        p.score = score_path(p, &plan.scoring);
    }
    paths.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    paths.truncate(plan.scoring.top_n.max(1));

    Ok(PathResult {
        paths,
        status: bfs.status,
    })
}

// ---------------------------------------------------------------------------
// Endpoint resolution.
// ---------------------------------------------------------------------------

fn resolve_endpoint(
    state: &PlanState,
    ctx: &ExecutorContext,
) -> Result<HashSet<MemoryId>, ExecError> {
    match state {
        PlanState::ByMemoryId(raw) => {
            let id = MemoryId::from(*raw);
            // Sub-task 9.16: spec §16/01 §12 — tombstoned memories
            // aren't visible unless explicitly requested. A
            // tombstoned start / goal returns an empty endpoint
            // set; `execute_path` short-circuits to `NoPathFound`.
            // Matches `search_active`'s silent-filter behavior for
            // ByText endpoints.
            if ctx.index.is_tombstoned(id) {
                return Ok(HashSet::new());
            }
            let mut s = HashSet::with_capacity(1);
            s.insert(id);
            Ok(s)
        }
        PlanState::ByText(text) => {
            let vector = ctx.embedder.embed(text)?;
            let hits =
                ctx.index
                    .search_active(&vector, ENDPOINT_RECALL_K, Some(ENDPOINT_RECALL_EF));
            Ok(hits.into_iter().map(|(id, _)| id).collect())
        }
        PlanState::ByVector { .. } => Err(ExecError::Unsupported(
            "PLAN endpoint ByVector — wire vector window not yet exposed to the executor",
        )),
    }
}

// ---------------------------------------------------------------------------
// Bidirectional BFS.
// ---------------------------------------------------------------------------

/// Parent-pointer entry. `parent = None` for the seed (frontier
/// origin); otherwise `(prev_node, edge_into_this_node, weight)`.
#[derive(Clone, Copy, Debug)]
struct Crumb {
    parent: Option<MemoryId>,
    edge: Option<EdgeKind>,
    edge_weight: f32,
    depth: usize,
}

struct BfsRaw {
    /// Each path is a `(nodes, edges, edge_weights)` chain from a
    /// start to a goal.
    paths: Vec<(Vec<MemoryId>, Vec<EdgeKind>, Vec<f32>)>,
    status: PlanStatus,
}

#[allow(clippy::too_many_arguments)]
fn run_bidirectional_bfs(
    starts: &HashSet<MemoryId>,
    goals: &HashSet<MemoryId>,
    max_depth: usize,
    edge_kinds: &HashSet<EdgeKind>,
    max_branches: usize,
    max_wall_time_ms: u64,
    max_paths: usize,
    ctx: &ExecutorContext,
) -> Result<BfsRaw, ExecError> {
    // Quick win: any start == any goal.
    let trivial: Vec<MemoryId> = starts.intersection(goals).copied().collect();
    if !trivial.is_empty() {
        return Ok(BfsRaw {
            paths: trivial
                .into_iter()
                .map(|id| (vec![id], Vec::new(), Vec::new()))
                .collect(),
            status: PlanStatus::GoalReached,
        });
    }

    let started = Instant::now();
    let wall_ms = max_wall_time_ms;
    let mut nodes_explored = 0usize;

    // Visited maps with parent pointers, one per side.
    let mut fwd: HashMap<MemoryId, Crumb> = HashMap::new();
    let mut bwd: HashMap<MemoryId, Crumb> = HashMap::new();
    let mut fwd_q: VecDeque<MemoryId> = VecDeque::new();
    let mut bwd_q: VecDeque<MemoryId> = VecDeque::new();

    for &s in starts {
        fwd.insert(
            s,
            Crumb {
                parent: None,
                edge: None,
                edge_weight: 1.0,
                depth: 0,
            },
        );
        fwd_q.push_back(s);
        nodes_explored += 1;
    }
    for &g in goals {
        bwd.insert(
            g,
            Crumb {
                parent: None,
                edge: None,
                edge_weight: 1.0,
                depth: 0,
            },
        );
        bwd_q.push_back(g);
        nodes_explored += 1;
    }

    let mut meeting_points: Vec<MemoryId> = Vec::new();
    let mut status = PlanStatus::NoPathFound;

    // Open one read txn for the whole BFS — repeated `read_txn()` calls
    // are cheap but not free, and the BFS may do hundreds of lookups.
    let metadata_guard = ctx.metadata.lock();
    let rtxn = metadata_guard
        .read_txn()
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
    let edges_out = rtxn
        .open_table(EDGES_OUT_TABLE)
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
    let edges_in = rtxn
        .open_table(EDGES_IN_TABLE)
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;

    // Alternate expansion of the smaller frontier until depth budget
    // is exhausted on both sides.
    while !fwd_q.is_empty() && !bwd_q.is_empty() {
        if started.elapsed().as_millis() as u64 > wall_ms {
            status = PlanStatus::Timeout;
            break;
        }
        if nodes_explored >= max_branches {
            status = PlanStatus::BudgetExhausted;
            break;
        }

        // Pick the smaller frontier; expand one BFS level.
        let expand_forward = fwd_q.len() <= bwd_q.len();
        let (queue, visited, other_visited, is_forward) = if expand_forward {
            (&mut fwd_q, &mut fwd, &bwd, true)
        } else {
            (&mut bwd_q, &mut bwd, &fwd, false)
        };

        let level_size = queue.len();
        for _ in 0..level_size {
            let node = match queue.pop_front() {
                Some(n) => n,
                None => break,
            };
            let crumb = visited[&node];
            if crumb.depth >= max_depth {
                continue;
            }

            // Fetch neighbours along this direction.
            let mut neighbours: Vec<(EdgeKind, MemoryId, f32)> = if is_forward {
                list_edges_from(&edges_out, node, None)
                    .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?
                    .into_iter()
                    .filter(|(k, _, _)| edge_kinds.contains(k))
                    .map(|(k, t, data)| (k, t, data.weight))
                    .collect()
            } else {
                list_edges_to(&edges_in, node, None)
                    .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?
                    .into_iter()
                    .filter(|(k, _, _)| edge_kinds.contains(k))
                    .map(|(k, s, data)| (k, s, data.weight))
                    .collect()
            };

            // Sub-task 9.16: drop committed tombstoned memories from
            // PLAN traversals (spec §16/01 §12). Outside an active
            // txn this is the only filter; inside one, the
            // `snap.tombstoned` retain below layers in-flight
            // tombstones on top.
            neighbours.retain(|(_, other, _)| !ctx.index.is_tombstoned(*other));

            // Layer the txn snapshot on top: add pending links matching
            // this direction; remove pending unlinks. Spec §09/08 §5.
            if let Some(snap) = &ctx.txn {
                if is_forward {
                    for (src, kind, tgt, w) in &snap.pending_links {
                        if *src == node && edge_kinds.contains(kind) {
                            neighbours.push((*kind, *tgt, *w));
                        }
                    }
                    neighbours.retain(|(k, t, _)| !snap.pending_unlinks.contains(&(node, *k, *t)));
                } else {
                    for (src, kind, tgt, w) in &snap.pending_links {
                        if *tgt == node && edge_kinds.contains(kind) {
                            neighbours.push((*kind, *src, *w));
                        }
                    }
                    neighbours.retain(|(k, s, _)| !snap.pending_unlinks.contains(&(*s, *k, node)));
                }
                // Drop in-flight tombstones (committed ones already
                // dropped above).
                neighbours.retain(|(_, other, _)| !snap.tombstoned.contains(other));
            }

            for (kind, next, weight) in neighbours {
                if visited.contains_key(&next) {
                    continue; // already seen on this side → skip
                }
                visited.insert(
                    next,
                    Crumb {
                        parent: Some(node),
                        edge: Some(kind),
                        edge_weight: weight,
                        depth: crumb.depth + 1,
                    },
                );
                queue.push_back(next);
                nodes_explored += 1;

                if other_visited.contains_key(&next) {
                    meeting_points.push(next);
                    if meeting_points.len() >= max_paths {
                        break;
                    }
                }

                if nodes_explored >= max_branches {
                    status = PlanStatus::BudgetExhausted;
                    break;
                }
            }
            if meeting_points.len() >= max_paths || status != PlanStatus::NoPathFound {
                break;
            }
        }

        if !meeting_points.is_empty() {
            status = PlanStatus::GoalReached;
            break;
        }
    }

    // Reconstruct paths from each meeting point.
    let paths = meeting_points
        .into_iter()
        .filter_map(|m| reconstruct(m, &fwd, &bwd))
        .collect();

    Ok(BfsRaw { paths, status })
}

/// Walk parent pointers from a meeting node out to the seeds on
/// both sides; stitch them into a forward-oriented
/// `(nodes, edges, edge_weights)` chain. Self-loops are impossible
/// because visited maps reject re-entry; we assert it anyway.
fn reconstruct(
    meet: MemoryId,
    fwd: &HashMap<MemoryId, Crumb>,
    bwd: &HashMap<MemoryId, Crumb>,
) -> Option<(Vec<MemoryId>, Vec<EdgeKind>, Vec<f32>)> {
    // Forward side: walk from meet → seed via fwd parents.
    let mut fwd_nodes = vec![meet];
    let mut fwd_edges: Vec<EdgeKind> = Vec::new();
    let mut fwd_weights: Vec<f32> = Vec::new();
    let mut cur = meet;
    while let Some(c) = fwd.get(&cur) {
        match c.parent {
            None => break,
            Some(p) => {
                fwd_nodes.push(p);
                fwd_edges.push(c.edge?);
                fwd_weights.push(c.edge_weight);
                cur = p;
            }
        }
    }
    fwd_nodes.reverse();
    fwd_edges.reverse();
    fwd_weights.reverse();

    // Backward side: walk from meet → goal seed via bwd parents.
    // `c.edge` on the bwd map is the edge `(c.parent → cur)` running
    // backward — i.e. in the forward orientation it's actually
    // `(cur → c.parent)` with the same `EdgeKind`.
    let mut bwd_nodes: Vec<MemoryId> = Vec::new();
    let mut bwd_edges: Vec<EdgeKind> = Vec::new();
    let mut bwd_weights: Vec<f32> = Vec::new();
    let mut cur = meet;
    while let Some(c) = bwd.get(&cur) {
        match c.parent {
            None => break,
            Some(p) => {
                bwd_nodes.push(p);
                bwd_edges.push(c.edge?);
                bwd_weights.push(c.edge_weight);
                cur = p;
            }
        }
    }

    // Stitch: forward chain ends at meet; backward chain starts at meet.
    let mut nodes = fwd_nodes;
    nodes.extend(bwd_nodes);
    let mut edges = fwd_edges;
    edges.extend(bwd_edges);
    let mut weights = fwd_weights;
    weights.extend(bwd_weights);

    // Self-loop guard (§16). Visited maps make this redundant on each
    // side, but the meet-point can theoretically appear twice if the
    // BFS finds the same node via both sides' seeds; assert.
    let mut seen = HashSet::with_capacity(nodes.len());
    for n in &nodes {
        if !seen.insert(*n) {
            return None; // discard; self-loop slipped through
        }
    }

    Some((nodes, edges, weights))
}

// ---------------------------------------------------------------------------
// Hydration: pull salience + text for the wire frame.
// ---------------------------------------------------------------------------

fn hydrate_paths(
    raw: Vec<(Vec<MemoryId>, Vec<EdgeKind>, Vec<f32>)>,
    ctx: &ExecutorContext,
) -> Result<Vec<Path>, ExecError> {
    let metadata_guard = ctx.metadata.lock();
    let rtxn = metadata_guard
        .read_txn()
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
    let table = rtxn
        .open_table(MEMORIES_TABLE)
        .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;

    let mut out = Vec::with_capacity(raw.len());
    for (nodes, edges, edge_weights) in raw {
        let mut sal = Vec::with_capacity(nodes.len());
        let text = vec![String::new(); nodes.len()];
        for &id in &nodes {
            // Look up committed first; fall back to the txn snapshot
            // (in-flight memory rows live there, not in redb yet).
            let row = table
                .get(id.to_be_bytes())
                .map_err(|e| ExecError::MetadataReadFailed(e.to_string()))?;
            let salience = if let Some(access) = row {
                access.value().salience
            } else if let Some(pending) = ctx
                .txn
                .as_ref()
                .and_then(|snap| snap.pending_memories.get(&id))
            {
                pending.salience
            } else {
                return Err(ExecError::MemoryNotFound { memory_id: id });
            };
            sal.push(salience);
        }
        out.push(Path {
            nodes,
            edges,
            edge_weights,
            score: 0.0,
            node_salience: sal,
            node_text: text,
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Scoring.
// ---------------------------------------------------------------------------

fn geomean(xs: &[f32]) -> f32 {
    if xs.is_empty() {
        return 1.0;
    }
    let mut log_sum = 0.0_f32;
    let mut n = 0u32;
    for &x in xs {
        if x <= 0.0 {
            // Any zero factor collapses the geomean to 0.
            return 0.0;
        }
        log_sum += x.ln();
        n += 1;
    }
    (log_sum / n as f32).exp()
}

fn score_path(p: &Path, scoring: &crate::plan::path::ScoringStep) -> f32 {
    let length_score = if scoring.include_length_score {
        1.0 / (1.0 + p.edges.len() as f32)
    } else {
        1.0
    };
    // Edge-weight: geometric mean of the per-edge weights along the
    // path (spec §09/04 §10). For symmetric kinds like SimilarTo and
    // for Contradicts (which can be negative), we take absolute value
    // so the geomean stays well-defined; the magnitude is what
    // contributes to confidence.
    let edge_score = if scoring.include_edge_weight_score && !p.edge_weights.is_empty() {
        let abs: Vec<f32> = p.edge_weights.iter().map(|w| w.abs()).collect();
        geomean(&abs)
    } else {
        1.0
    };
    let salience_score = if scoring.include_salience_score {
        geomean(&p.node_salience)
    } else {
        1.0
    };
    length_score * edge_score * salience_score
}

// ---------------------------------------------------------------------------
// Tests live in `crates/brain-planner/tests/path_executor.rs`.
// ---------------------------------------------------------------------------
