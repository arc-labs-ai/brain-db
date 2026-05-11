//! EXPLAIN-style pretty-printer for execution plans.
//!
//! Operators read this output in logs or via the future
//! `ADMIN_EXPLAIN_PLAN` opcode (Phase 9). The format is human-only —
//! no parser, no machine consumer.
//!
//! Renders as a tree using ASCII box-drawing characters
//! (`├─ │ └─`). Each plan-type's title line carries `(est. X.YZ ms)`
//! so cost is visible without scanning.
//!
//! Phase doc 6.8 said "impl Debug"; we ship `Display` instead because
//! the derive-generated `Debug` is still used in test panic messages
//! (`assert!(format!("{plan:?}").contains(...))`). Overriding `Debug`
//! would break that. `Display` is the idiomatic home for human
//! formatting in Rust.

use std::fmt;

use crate::plan::EncodePlan;
use crate::plan::{
    common::RecallSubStep, encode::ContextResolutionStep, forget::ForgetPlan, path::PathPlan,
    reason::ReasonPlan, recall::RecallPlan, ExecutionPlan,
};

const INDENT: &str = "   ";
const BRANCH: &str = "├─ ";
const LAST: &str = "└─ ";
const VBAR: &str = "│  ";

/// Convenience: render a plan as a string.
#[must_use]
pub fn explain(plan: &ExecutionPlan) -> String {
    format!("{plan}")
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        // Find a char boundary at or below `max - 3` to avoid splitting
        // a multi-byte character.
        let mut cut = max.saturating_sub(3);
        while cut > 0 && !text.is_char_boundary(cut) {
            cut -= 1;
        }
        format!("{}...", &text[..cut])
    }
}

fn hex_prefix(bytes: &[u8], n: usize) -> String {
    let n = n.min(bytes.len());
    let mut s = String::with_capacity(n * 2 + 1);
    for b in &bytes[..n] {
        s.push_str(&format!("{b:02x}"));
    }
    s.push('…');
    s
}

fn cost_suffix(ms: f32) -> String {
    format!("(est. {ms:.2} ms)")
}

// ---------------------------------------------------------------------------
// ExecutionPlan — dispatches to per-variant Display.
// ---------------------------------------------------------------------------

impl fmt::Display for ExecutionPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecutionPlan::Recall(p) => write!(f, "{p}"),
            ExecutionPlan::Encode(p) => write!(f, "{p}"),
            ExecutionPlan::Forget(p) => write!(f, "{p}"),
            ExecutionPlan::Plan(p) => write!(f, "{p}"),
            ExecutionPlan::Reason(p) => write!(f, "{p}"),
        }
    }
}

// ---------------------------------------------------------------------------
// RecallPlan.
// ---------------------------------------------------------------------------

impl fmt::Display for RecallPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "RecallPlan  {}", cost_suffix(self.estimated_cost_ms))?;
        writeln!(
            f,
            "{BRANCH}embedding: \"{}\" (cache_lookup={})",
            truncate(&self.embedding.text, 40),
            self.embedding.cache_lookup
        )?;
        writeln!(f, "{BRANCH}shards ({})", self.shards.len())?;
        let last_shard = self.shards.len().saturating_sub(1);
        for (i, shard) in self.shards.iter().enumerate() {
            let s_branch = if i == last_shard { LAST } else { BRANCH };
            writeln!(
                f,
                "{VBAR}{s_branch}ShardSearchStep shard={}",
                shard.shard_id
            )?;
            let inner_indent = if i == last_shard {
                format!("{VBAR}{INDENT}")
            } else {
                format!("{VBAR}{VBAR}")
            };
            writeln!(
                f,
                "{inner_indent}{BRANCH}ann_search: ef={}, candidates={}",
                shard.ann_search.ef, shard.ann_search.candidates_to_request
            )?;
            writeln!(
                f,
                "{inner_indent}{BRANCH}metadata_lookup: include_extra={}",
                shard.metadata_lookup.include_extra
            )?;
            writeln!(
                f,
                "{inner_indent}{LAST}filter_apply: {:?}, {} rules",
                shard.filter_apply.stage,
                shard.filter_apply.rules.len()
            )?;
        }
        writeln!(
            f,
            "{BRANCH}merge: sort_by={:?}, final_top={}, confidence_min={:?}",
            self.merge.sort_by, self.merge.final_top, self.merge.confidence_min
        )?;
        match &self.text_fetch {
            None => writeln!(f, "{BRANCH}text_fetch: None")?,
            Some(t) => writeln!(
                f,
                "{BRANCH}text_fetch: Some, ids={}, parallel={}",
                t.memory_ids.len(),
                t.parallel
            )?,
        }
        write!(
            f,
            "{LAST}response: include_text={}, include_metadata={}",
            self.response.include_text, self.response.include_metadata
        )
    }
}

// ---------------------------------------------------------------------------
// EncodePlan.
// ---------------------------------------------------------------------------

impl fmt::Display for EncodePlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "EncodePlan shard={}  {}",
            self.shard,
            cost_suffix(self.estimated_cost_ms)
        )?;
        let rid_bytes: [u8; 16] = self.idempotency_check.request_id.into();
        writeln!(
            f,
            "{BRANCH}idempotency_check: request_id={}",
            hex_prefix(&rid_bytes, 4)
        )?;
        writeln!(
            f,
            "{BRANCH}embedding: \"{}\" (cache_lookup={})",
            truncate(&self.embedding.text, 40),
            self.embedding.cache_lookup
        )?;
        match &self.context_resolution {
            ContextResolutionStep::Explicit(id) => {
                writeln!(f, "{BRANCH}context_resolution: Explicit({id:?})")?;
            }
            ContextResolutionStep::GetOrCreate { agent_id: _, name } => {
                writeln!(
                    f,
                    "{BRANCH}context_resolution: GetOrCreate(\"{}\")",
                    truncate(name, 30)
                )?;
            }
        }
        writeln!(
            f,
            "{BRANCH}allocation: arena_grow_if_needed={}",
            self.allocation.arena_grow_if_needed
        )?;
        writeln!(
            f,
            "{BRANCH}wal_append: kind={:?}, salience_initial={:.2}, fsync={}",
            self.wal_append.kind, self.wal_append.salience_initial, self.wal_append.fsync
        )?;
        writeln!(
            f,
            "{BRANCH}apply: arena_write={}, metadata_write={}, hnsw_insert={}",
            self.apply.arena_write, self.apply.metadata_write, self.apply.hnsw_insert
        )?;
        writeln!(f, "{BRANCH}edges ({})", self.edges.len())?;
        write!(
            f,
            "{LAST}response: persistent_id={}",
            self.response.persistent_id
        )
    }
}

// ---------------------------------------------------------------------------
// ForgetPlan.
// ---------------------------------------------------------------------------

impl fmt::Display for ForgetPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "ForgetPlan shard={}  {}",
            self.shard,
            cost_suffix(self.estimated_cost_ms)
        )?;
        writeln!(
            f,
            "{BRANCH}memory_id={} mode={:?}",
            hex_prefix(&self.memory_id.to_be_bytes(), 8),
            self.mode
        )?;
        let rid_bytes: [u8; 16] = self.idempotency_check.request_id.into();
        writeln!(
            f,
            "{BRANCH}idempotency_check: request_id={}",
            hex_prefix(&rid_bytes, 4)
        )?;
        writeln!(
            f,
            "{BRANCH}wal_append: fsync={}, mode={:?}",
            self.wal_append.fsync, self.wal_append.mode
        )?;
        writeln!(
            f,
            "{BRANCH}apply: arena_tombstone={}, metadata_commit={}, hnsw_mark_removed={}, \
             arena_zero_vector={}, text_zero={}",
            self.apply.arena_tombstone,
            self.apply.metadata_commit,
            self.apply.hnsw_mark_removed,
            self.apply.arena_zero_vector,
            self.apply.text_zero
        )?;
        write!(
            f,
            "{LAST}response: include_outcome={}",
            self.response.include_outcome
        )
    }
}

// ---------------------------------------------------------------------------
// PathPlan.
// ---------------------------------------------------------------------------

impl fmt::Display for PathPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "PathPlan strategy={:?}  {}",
            self.strategy,
            cost_suffix(self.estimated_cost_ms)
        )?;
        writeln!(f, "{BRANCH}start: {:?}", self.start)?;
        writeln!(f, "{BRANCH}goal:  {:?}", self.goal)?;
        writeln!(
            f,
            "{BRANCH}budget: max_steps={}, max_branches={}, wall={} ms",
            self.budget.max_steps, self.budget.max_branches_explored, self.budget.max_wall_time_ms
        )?;
        writeln!(
            f,
            "{BRANCH}starting_recall: {}",
            substep_one_liner(&self.starting_recall)
        )?;
        writeln!(
            f,
            "{BRANCH}goal_recall:     {}",
            substep_one_liner(&self.goal_recall)
        )?;
        writeln!(
            f,
            "{BRANCH}traversal: max_depth={}, max_paths={}, bidirectional={}, kinds={:?}",
            self.traversal.max_depth,
            self.traversal.max_paths,
            self.traversal.bidirectional,
            self.traversal.edge_kinds
        )?;
        writeln!(
            f,
            "{BRANCH}scoring: length={}, edge_weight={}, salience={}, top_n={}",
            self.scoring.include_length_score,
            self.scoring.include_edge_weight_score,
            self.scoring.include_salience_score,
            self.scoring.top_n
        )?;
        write!(
            f,
            "{LAST}response: paths={}, text={}, metadata={}",
            self.response.include_paths, self.response.include_text, self.response.include_metadata
        )
    }
}

// ---------------------------------------------------------------------------
// ReasonPlan.
// ---------------------------------------------------------------------------

impl fmt::Display for ReasonPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "ReasonPlan depth={} max_inferences={}  {}",
            self.depth,
            self.max_inferences,
            cost_suffix(self.estimated_cost_ms)
        )?;
        writeln!(f, "{BRANCH}observation: {:?}", self.observation)?;
        writeln!(
            f,
            "{BRANCH}embedding: {}",
            self.embedding.as_ref().map_or("None", |_| "Some")
        )?;
        writeln!(
            f,
            "{BRANCH}base_recall: {}",
            substep_one_liner(&self.base_recall)
        )?;
        writeln!(
            f,
            "{BRANCH}supports_traversal: max_depth={}, kinds={:?}",
            self.supports_traversal.max_depth, self.supports_traversal.edge_kinds
        )?;
        writeln!(
            f,
            "{BRANCH}contradicts_traversal: max_depth={}, kinds={:?}",
            self.contradicts_traversal.max_depth, self.contradicts_traversal.edge_kinds
        )?;
        writeln!(
            f,
            "{BRANCH}aggregation: max_supporting={}, max_contradicting={}, aggregate={}",
            self.aggregation.max_supporting,
            self.aggregation.max_contradicting,
            self.aggregation.include_aggregate_confidence
        )?;
        write!(
            f,
            "{LAST}response: paths={}, text={}, metadata={}",
            self.response.include_paths, self.response.include_text, self.response.include_metadata
        )
    }
}

fn substep_one_liner(s: &Option<RecallSubStep>) -> &'static str {
    if s.is_some() {
        "Some"
    } else {
        "None"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{ContextId, MemoryId, MemoryKind, RequestId};
    use brain_protocol::request::{
        ForgetMode, ObservationInput, PlanBudget, PlanState, PlanStrategy,
    };

    use crate::plan::{
        common::EdgeSpec,
        encode::{
            ApplyStep, EdgeStep, EncodePlan, EncodeResponseStep, IdempotencyCheckStep,
            SlotAllocationStep, WalAppendStep,
        },
        forget::{ForgetApplyStep, ForgetPlan, ForgetResponseStep, ForgetWalStep},
        path::{
            default_plan_edge_kinds, EvidenceResponseStep, PathPlan, ScoringStep, TraversalStep,
        },
        reason::{
            default_contradicts_edge_kinds, default_supports_edge_kinds, AggregationStep,
            ReasonPlan,
        },
        recall::{
            AnnSearchStep, EmbeddingStep, FilterStep, MergeStep, MetadataLookupStep, RecallPlan,
            ResponseStep, ShardSearchStep,
        },
        FilterStage, SortKey,
    };
    use brain_core::EdgeKind;

    fn sample_recall() -> RecallPlan {
        RecallPlan {
            embedding: EmbeddingStep {
                text: "the cat sat on the mat".into(),
                cache_lookup: true,
            },
            shards: vec![ShardSearchStep {
                shard_id: 0,
                ann_search: AnnSearchStep {
                    ef: 64,
                    candidates_to_request: 80,
                    pre_filter: vec![],
                },
                metadata_lookup: MetadataLookupStep {
                    include_extra: false,
                },
                filter_apply: FilterStep {
                    stage: FilterStage::PostFilter,
                    rules: vec![],
                },
            }],
            merge: MergeStep {
                sort_by: SortKey::Score,
                final_top: 10,
                confidence_min: None,
            },
            text_fetch: None,
            response: ResponseStep {
                include_text: false,
                include_metadata: false,
            },
            estimated_cost_ms: 7.51,
        }
    }

    fn sample_encode() -> EncodePlan {
        EncodePlan {
            shard: 0,
            idempotency_check: IdempotencyCheckStep {
                request_id: RequestId::from([0x01; 16]),
            },
            embedding: EmbeddingStep {
                text: "hello".into(),
                cache_lookup: true,
            },
            context_resolution: crate::plan::encode::ContextResolutionStep::Explicit(ContextId(42)),
            allocation: SlotAllocationStep {
                arena_grow_if_needed: true,
            },
            wal_append: WalAppendStep {
                kind: MemoryKind::Episodic,
                salience_initial: 0.5,
                fsync: true,
            },
            apply: ApplyStep {
                arena_write: true,
                metadata_write: true,
                hnsw_insert: true,
            },
            edges: vec![EdgeStep {
                edge: EdgeSpec {
                    target: MemoryId::from(7u128),
                    kind: EdgeKind::Caused,
                    weight: 0.5,
                },
                insert_in_metadata: true,
            }],
            response: EncodeResponseStep {
                persistent_id: true,
            },
            estimated_cost_ms: 9.86,
        }
    }

    fn sample_forget() -> ForgetPlan {
        ForgetPlan {
            shard: 0,
            memory_id: MemoryId::from(7u128),
            mode: ForgetMode::Soft,
            idempotency_check: IdempotencyCheckStep {
                request_id: RequestId::from([0x01; 16]),
            },
            wal_append: ForgetWalStep {
                fsync: true,
                mode: ForgetMode::Soft,
            },
            apply: ForgetApplyStep {
                arena_tombstone: true,
                metadata_commit: true,
                hnsw_mark_removed: true,
                arena_zero_vector: false,
                text_zero: false,
            },
            response: ForgetResponseStep {
                include_outcome: true,
            },
            estimated_cost_ms: 0.80,
        }
    }

    fn sample_path() -> PathPlan {
        PathPlan {
            start: PlanState::ByText("origin".into()),
            goal: PlanState::ByText("destination".into()),
            budget: PlanBudget {
                max_steps: 4,
                max_wall_time_ms: 100,
                max_branches_explored: 64,
            },
            strategy: PlanStrategy::Auto,
            starting_recall: None,
            goal_recall: None,
            traversal: TraversalStep {
                edge_kinds: default_plan_edge_kinds(),
                max_depth: 4,
                bidirectional: true,
                max_paths: 64,
            },
            scoring: ScoringStep::default(),
            response: EvidenceResponseStep {
                include_paths: true,
                include_text: false,
                include_metadata: false,
            },
            estimated_cost_ms: 24.30,
        }
    }

    fn sample_reason() -> ReasonPlan {
        ReasonPlan {
            observation: ObservationInput::ByText("the cat sat".into()),
            depth: 3,
            confidence_threshold: 0.5,
            max_inferences: 5,
            budget_wall_time_ms: 100,
            embedding: None,
            base_recall: None,
            supports_traversal: TraversalStep {
                edge_kinds: default_supports_edge_kinds(),
                max_depth: 3,
                bidirectional: false,
                max_paths: 8,
            },
            contradicts_traversal: TraversalStep {
                edge_kinds: default_contradicts_edge_kinds(),
                max_depth: 3,
                bidirectional: false,
                max_paths: 8,
            },
            aggregation: AggregationStep::default(),
            response: EvidenceResponseStep {
                include_paths: true,
                include_text: false,
                include_metadata: false,
            },
            estimated_cost_ms: 12.40,
        }
    }

    #[test]
    fn recall_renders_tree_with_key_substrings() {
        let s = format!("{}", sample_recall());
        assert!(s.starts_with("RecallPlan"), "title line, got: {s}");
        assert!(s.contains("est. 7.51 ms"));
        assert!(s.contains("├─"));
        assert!(s.contains("└─"));
        assert!(s.contains("embedding"));
        assert!(s.contains("ann_search"));
        assert!(s.contains("ef=64"));
        assert!(s.contains("merge"));
        assert!(s.contains("response"));
        assert!(s.contains("text_fetch: None"));
    }

    #[test]
    fn encode_renders_tree() {
        let s = format!("{}", sample_encode());
        assert!(s.starts_with("EncodePlan"));
        assert!(s.contains("est. 9.86 ms"));
        assert!(s.contains("idempotency_check"));
        assert!(s.contains("01010101"), "first hex bytes of request_id");
        assert!(s.contains("Explicit"));
        assert!(s.contains("wal_append"));
        assert!(s.contains("Episodic"));
        assert!(s.contains("edges (1)"));
    }

    #[test]
    fn forget_renders_tree() {
        let s = format!("{}", sample_forget());
        assert!(s.starts_with("ForgetPlan"));
        assert!(s.contains("mode=Soft"));
        assert!(s.contains("arena_zero_vector=false"));
    }

    #[test]
    fn forget_hard_renders_zeroing() {
        let mut f = sample_forget();
        f.mode = ForgetMode::Hard;
        f.wal_append.mode = ForgetMode::Hard;
        f.apply.arena_zero_vector = true;
        f.apply.text_zero = true;
        let s = format!("{f}");
        assert!(s.contains("mode=Hard"));
        assert!(s.contains("arena_zero_vector=true"));
        assert!(s.contains("text_zero=true"));
    }

    #[test]
    fn path_renders_tree() {
        let s = format!("{}", sample_path());
        assert!(s.starts_with("PathPlan"));
        assert!(s.contains("strategy=Auto"));
        assert!(s.contains("max_steps=4"));
        assert!(s.contains("Caused"));
        assert!(s.contains("FollowedBy"));
        assert!(s.contains("starting_recall: None"));
    }

    #[test]
    fn reason_renders_tree() {
        let s = format!("{}", sample_reason());
        assert!(s.starts_with("ReasonPlan"));
        assert!(s.contains("depth=3"));
        assert!(s.contains("Supports"));
        assert!(s.contains("DerivedFrom"));
        assert!(s.contains("Contradicts"));
        assert!(s.contains("max_supporting=5"));
    }

    #[test]
    fn execution_plan_dispatches_to_variant() {
        let plan = ExecutionPlan::Recall(sample_recall());
        let s = explain(&plan);
        assert!(s.contains("RecallPlan"));
    }

    #[test]
    fn long_text_is_truncated() {
        let mut p = sample_recall();
        p.embedding.text = "a".repeat(200);
        let s = format!("{p}");
        // 40-char cap → 37 chars + "..."
        assert!(s.contains("..."));
        assert!(!s.contains(&"a".repeat(50)));
    }
}
