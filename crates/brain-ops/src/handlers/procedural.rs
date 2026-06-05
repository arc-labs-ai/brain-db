//! `MATERIALIZE_PROCEDURAL` handler (W3.1, wire v2).
//!
//! Reads the calling agent's stored `brain:behavior_*` Preferences,
//! sorts by confidence, applies the `top_k` cap, and renders a single
//! Markdown system block ready for LLM prompt injection.
//!
//! The handler is read-only: it opens an `rtxn`, walks the predicate
//! registry to learn which ids belong to the procedural namespace,
//! and dispatches per-predicate listings through the existing
//! `statement_list` index. No writes, no extractor calls.

use std::collections::HashMap;

use brain_core::{ContextId, EntityId, PredicateId, StatementKind};
use brain_core::{Statement, StatementObject, StatementValue};
use brain_metadata::schema::predicate::{predicate_get, predicate_lookup_by_qname};
use brain_metadata::statement::{statement_list, StatementListFilter};
use brain_protocol::{MaterializeProceduralRequest, MaterializeProceduralResponse};

use crate::context::OpsContext;
use crate::error::OpError;

/// Hard cap on `top_k`. Bigger requests get rejected up-front so a
/// runaway caller can't burn an unbounded amount of CPU rendering a
/// system block that wouldn't fit a real prompt anyway.
const TOP_K_MAX: u32 = 100;

/// Default applied when the client passes `top_k = 0`.
const TOP_K_DEFAULT: u32 = 20;

/// Soft cap on `categories` payload — purely a defensive bound on
/// wire input. Each entry is a short word (e.g. `"tone"`); 32 is
/// well past anything sensible.
const CATEGORIES_MAX: usize = 32;

/// Longest accepted category suffix. Defensive — predicate names are
/// short identifiers in practice.
const CATEGORY_LEN_MAX: usize = 64;

/// All five `brain:behavior_*` predicate names declared in
/// `system_schema/schema.brain`. Held as a static array so the renderer
/// can group them into the three sections (tone/style, preferences,
/// constraints) without re-deriving from configuration.
const BEHAVIOR_PREDICATES: &[&str] = &[
    "behavior_tone",
    "behavior_style",
    "behavior_prefers",
    "behavior_avoids",
    "behavior_constraint",
];

const BEHAVIOR_PREFIX: &str = "behavior_";
const BEHAVIOR_NAMESPACE: &str = "brain";

/// Render section assignment for a single behavior predicate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Section {
    /// `behavior_tone` + `behavior_style`.
    ToneAndStyle,
    /// `behavior_prefers`.
    Preferences,
    /// `behavior_avoids` + `behavior_constraint`.
    Constraints,
}

fn section_for(name: &str) -> Section {
    match name {
        "behavior_tone" | "behavior_style" => Section::ToneAndStyle,
        "behavior_prefers" => Section::Preferences,
        "behavior_avoids" | "behavior_constraint" => Section::Constraints,
        _ => Section::Preferences,
    }
}

pub async fn handle_materialize_procedural(
    req: MaterializeProceduralRequest,
    ctx: &OpsContext,
) -> Result<MaterializeProceduralResponse, OpError> {
    // ── Validation ────────────────────────────────────────────────
    if req.top_k > TOP_K_MAX {
        return Err(OpError::InvalidRequest(format!(
            "top_k must be in 0..={TOP_K_MAX} (0 means default)"
        )));
    }
    if req.min_confidence.is_nan() || !(0.0..=1.0).contains(&req.min_confidence) {
        return Err(OpError::InvalidRequest(
            "min_confidence must be a number in [0, 1]".into(),
        ));
    }
    if req.categories.len() > CATEGORIES_MAX {
        return Err(OpError::InvalidRequest(format!(
            "categories list capped at {CATEGORIES_MAX} entries"
        )));
    }
    for c in &req.categories {
        if c.is_empty() || c.len() > CATEGORY_LEN_MAX {
            return Err(OpError::InvalidRequest(
                "each category must be 1..=64 chars".into(),
            ));
        }
    }
    let effective_top_k = if req.top_k == 0 {
        TOP_K_DEFAULT
    } else {
        req.top_k
    };

    // The wire field is opt-in; an all-zeros agent_id means "use the
    // authenticated caller". Anonymous deployments fall back to
    // AgentId::NIL which won't have any procedural statements stored
    // against it — the renderer returns an empty block in that case.
    let agent_bytes = if req.agent_id == [0u8; 16] {
        ctx.executor.caller_agent.0.into_bytes()
    } else {
        req.agent_id
    };
    let subject_entity = EntityId::from(agent_bytes);

    let context_filter = if req.context_filter == 0 {
        None
    } else {
        Some(ContextId(req.context_filter))
    };

    // ── Resolve the procedural predicate set ─────────────────────
    // Walks the registry once per call (5 lookups). When a schema
    // hasn't been seeded the predicate rows won't exist and we
    // return an empty block — no agent could have written a
    // procedural statement without those predicates declared.
    let categories_set: Option<&[String]> = if req.categories.is_empty() {
        None
    } else {
        Some(req.categories.as_slice())
    };

    let (matched_rows, total_candidates) = {
        let rtxn = ctx
            .executor
            .metadata
            .read_txn()
            .map_err(|e| OpError::Internal(format!("read_txn: {e}")))?;

        // Resolve the procedural predicates that exist in the registry.
        // Missing rows are skipped — they're a deployment-config story
        // (the operator hasn't seeded the system schema), not a
        // per-request error.
        let mut procedural_ids: HashMap<PredicateId, String> = HashMap::new();
        for name in BEHAVIOR_PREDICATES {
            if !category_matches(name, categories_set) {
                continue;
            }
            match predicate_lookup_by_qname(&rtxn, BEHAVIOR_NAMESPACE, name)
                .map_err(OpError::from)?
            {
                Some(p) => {
                    procedural_ids.insert(p.id, (*name).into());
                }
                None => {
                    tracing::debug!(
                        predicate = name,
                        "procedural predicate not registered; system schema may not be seeded yet",
                    );
                }
            }
        }

        if procedural_ids.is_empty() {
            return Ok(MaterializeProceduralResponse {
                system_block: render_empty_block(),
                statement_ids: Vec::new(),
                total_candidates: 0,
                trimmed_by_budget: false,
            });
        }

        // List statements per predicate (uses the
        // `STATEMENTS_BY_SUBJECT_TABLE` narrow index when both
        // subject and predicate are bound). Aggregate then sort.
        let mut hits: Vec<Statement> = Vec::new();
        for &pid in procedural_ids.keys() {
            let filter = StatementListFilter {
                subject: Some(subject_entity),
                predicate: Some(pid),
                kind: Some(StatementKind::Preference),
                current_only: true,
                min_confidence: if req.min_confidence > 0.0 {
                    Some(req.min_confidence)
                } else {
                    None
                },
                // Pull the full active set for this predicate; we
                // sort + cap across the union below.
                limit: TOP_K_MAX as usize,
            };
            let rows = statement_list(&rtxn, &filter).map_err(OpError::from)?;
            for row in rows {
                if row.tombstoned {
                    continue;
                }
                if row.superseded_by.is_some() {
                    continue;
                }
                if let Some(ctx_id) = context_filter {
                    if !statement_touches_context(&row, ctx_id) {
                        continue;
                    }
                }
                hits.push(row);
            }
        }

        let total_candidates = hits.len() as u32;

        // Confidence-desc, then extracted_at_unix_nanos-desc as a
        // stable tie-break (newer evidence wins).
        hits.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.extracted_at_unix_nanos.cmp(&a.extracted_at_unix_nanos))
        });

        let cap = effective_top_k as usize;
        if hits.len() > cap {
            hits.truncate(cap);
        }

        // Project per-row render data while we still hold the rtxn —
        // need the predicate name (the registry lookup needs the
        // PredicateId → name map) and the rendered object text.
        let mut rendered: Vec<RenderedStatement> = Vec::with_capacity(hits.len());
        for s in hits {
            let predicate_name = match procedural_ids.get(&s.predicate) {
                Some(name) => name.clone(),
                None => match predicate_get(&rtxn, s.predicate).map_err(OpError::from)? {
                    Some(p) => p.name,
                    None => continue,
                },
            };
            let Some(object_text) = render_object(&s.object) else {
                // Non-text objects don't belong in a procedural
                // system block — drop them rather than render an
                // opaque "<entity>" placeholder.
                continue;
            };
            rendered.push(RenderedStatement {
                statement_id: s.id.to_bytes(),
                predicate_name,
                object_text,
                confidence: s.confidence,
            });
        }

        (rendered, total_candidates)
    };

    let trimmed_by_budget = total_candidates > effective_top_k;
    let statement_ids = matched_rows.iter().map(|r| r.statement_id).collect();
    let system_block = render_system_block(&matched_rows, total_candidates);

    Ok(MaterializeProceduralResponse {
        system_block,
        statement_ids,
        total_candidates,
        trimmed_by_budget,
    })
}

/// A single rendered procedural statement, ready to feed the section
/// splitter. Decoupled from `brain_core::Statement` so the renderer
/// doesn't need the rtxn lifetime.
struct RenderedStatement {
    statement_id: [u8; 16],
    predicate_name: String,
    object_text: String,
    confidence: f32,
}

fn category_matches(predicate_name: &str, allow: Option<&[String]>) -> bool {
    let Some(list) = allow else { return true };
    let suffix = predicate_name
        .strip_prefix(BEHAVIOR_PREFIX)
        .unwrap_or(predicate_name);
    list.iter().any(|c| c.eq_ignore_ascii_case(suffix))
}

fn render_object(obj: &StatementObject) -> Option<String> {
    match obj {
        StatementObject::Value(StatementValue::Text(s)) => {
            let trimmed = s.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_string())
            }
        }
        _ => None,
    }
}

fn statement_touches_context(s: &Statement, context: ContextId) -> bool {
    // Procedural memory is inherently agent-scoped; the per-memory
    // ContextId lives in the `memories` redb row, not on the
    // statement itself, so a precise filter would require an
    // O(n_evidence) row-by-row lookup against the rtxn. For v1 we
    // treat the context filter as advisory and accept every row —
    // an agent's `behavior_*` claim doesn't shift meaning across
    // contexts the way a Fact would. A later pass can fold per-
    // evidence context lookups in if a use case emerges.
    let _ = (s, context);
    true
}

fn render_system_block(rows: &[RenderedStatement], total_candidates: u32) -> String {
    if rows.is_empty() {
        return render_empty_block();
    }

    let mut tone: Vec<&RenderedStatement> = Vec::new();
    let mut prefs: Vec<&RenderedStatement> = Vec::new();
    let mut constraints: Vec<&RenderedStatement> = Vec::new();
    for r in rows {
        match section_for(&r.predicate_name) {
            Section::ToneAndStyle => tone.push(r),
            Section::Preferences => prefs.push(r),
            Section::Constraints => constraints.push(r),
        }
    }

    let mut out = String::new();
    out.push_str("# Learned behaviors (procedural memory)\n\n");
    out.push_str("The following are behaviors the agent has learned over prior sessions.\n");
    out.push_str(
        "They are sorted by confidence; ignore any that seem inconsistent with the current request.\n\n",
    );

    push_section(&mut out, "Tone & style", &tone);
    push_section(&mut out, "Preferences", &prefs);
    push_section(&mut out, "Constraints", &constraints);

    // Provenance footer — first five statement ids in rendering
    // order so callers can debug surprising behavior without
    // round-tripping through the audit log.
    out.push_str("(generated from ");
    out.push_str(&rows.len().to_string());
    out.push_str(" active procedural statement");
    if rows.len() != 1 {
        out.push('s');
    }
    if total_candidates as usize > rows.len() {
        out.push_str(", trimmed from ");
        out.push_str(&total_candidates.to_string());
    }
    out.push_str("; ids for audit: ");
    let take = rows.len().min(5);
    for (i, r) in rows.iter().take(take).enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&uuid_short(&r.statement_id));
    }
    if rows.len() > take {
        out.push_str(", …");
    }
    out.push_str(")\n");

    out
}

fn push_section(out: &mut String, title: &str, rows: &[&RenderedStatement]) {
    if rows.is_empty() {
        return;
    }
    out.push_str("## ");
    out.push_str(title);
    out.push_str("\n\n");
    for r in rows {
        out.push_str("- ");
        out.push_str(&r.object_text);
        out.push_str(" (");
        out.push_str(&format_confidence(r.confidence));
        out.push_str(")\n");
    }
    out.push('\n');
}

fn render_empty_block() -> String {
    "# Learned behaviors (procedural memory)\n\n\
     (no procedural statements stored for this agent yet)\n"
        .to_string()
}

fn format_confidence(c: f32) -> String {
    format!("{:.2}", c.clamp(0.0, 1.0))
}

fn uuid_short(b: &[u8; 16]) -> String {
    // Mirrors the shell's short-id rendering: first 8 hex chars.
    let mut s = String::with_capacity(8);
    for byte in &b[..4] {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

// Errors are classified by `OpError`'s `From` impls (crate::error) —
// this path previously downgraded everything to Internal (500), losing
// NotFound/Conflict classification; routing through `From` fixes that.

// ---------------------------------------------------------------------------
// Tests (rendering / validation only — handler integration tests live in
// `tests/procedural.rs` since they need the full OpsContext fixture).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_row(name: &str, object: &str, confidence: f32, seed: u8) -> RenderedStatement {
        RenderedStatement {
            statement_id: [seed; 16],
            predicate_name: name.into(),
            object_text: object.into(),
            confidence,
        }
    }

    #[test]
    fn render_empty_block_for_no_rows() {
        let out = render_system_block(&[], 0);
        assert!(out.contains("no procedural statements"));
        assert!(out.starts_with("# Learned behaviors"));
    }

    #[test]
    fn render_groups_predicates_into_sections() {
        let rows = vec![
            sample_row("behavior_tone", "be concise", 0.9, 1),
            sample_row("behavior_prefers", "use bullets", 0.8, 2),
            sample_row("behavior_avoids", "no jargon", 0.7, 3),
            sample_row("behavior_constraint", "never reveal keys", 0.95, 4),
        ];
        let out = render_system_block(&rows, 4);
        assert!(out.contains("## Tone & style"));
        assert!(out.contains("be concise"));
        assert!(out.contains("## Preferences"));
        assert!(out.contains("use bullets"));
        assert!(out.contains("## Constraints"));
        assert!(out.contains("no jargon"));
        assert!(out.contains("never reveal keys"));
        // Confidence is rendered alongside each bullet.
        assert!(out.contains("(0.90)"));
        assert!(out.contains("(0.95)"));
    }

    #[test]
    fn render_includes_audit_footer_with_trim_count() {
        let rows = vec![
            sample_row("behavior_tone", "first", 0.9, 1),
            sample_row("behavior_tone", "second", 0.8, 2),
        ];
        let out = render_system_block(&rows, 5);
        // 2 of 5 rendered.
        assert!(out.contains("trimmed from 5"));
        assert!(out.contains("ids for audit"));
        // First 8 hex of statement id 1 is "01010101".
        assert!(out.contains("01010101"));
    }

    #[test]
    fn render_caps_audit_ids_at_five() {
        let rows: Vec<RenderedStatement> = (0..7)
            .map(|i| sample_row("behavior_prefers", &format!("row {i}"), 0.5, i as u8))
            .collect();
        let out = render_system_block(&rows, 7);
        // Trailing ellipsis when more than 5 rows.
        assert!(out.contains("…"));
    }

    #[test]
    fn category_matches_strips_behavior_prefix() {
        let allow = vec!["tone".to_string(), "style".to_string()];
        assert!(category_matches("behavior_tone", Some(&allow)));
        assert!(category_matches("behavior_style", Some(&allow)));
        assert!(!category_matches("behavior_prefers", Some(&allow)));
        // Empty/None allowlist passes everything.
        assert!(category_matches("behavior_constraint", None));
    }

    #[test]
    fn section_assignment_matches_predicate_groups() {
        assert_eq!(section_for("behavior_tone"), Section::ToneAndStyle);
        assert_eq!(section_for("behavior_style"), Section::ToneAndStyle);
        assert_eq!(section_for("behavior_prefers"), Section::Preferences);
        assert_eq!(section_for("behavior_avoids"), Section::Constraints);
        assert_eq!(section_for("behavior_constraint"), Section::Constraints);
    }

    #[test]
    fn render_object_only_accepts_non_empty_text() {
        use brain_core::{StatementObject, StatementValue};
        assert_eq!(
            render_object(&StatementObject::Value(StatementValue::Text("hi".into()))),
            Some("hi".into())
        );
        assert_eq!(
            render_object(&StatementObject::Value(StatementValue::Text("  ".into()))),
            None
        );
        assert_eq!(
            render_object(&StatementObject::Value(StatementValue::Integer(5))),
            None
        );
    }

    #[test]
    fn format_confidence_clamps_and_rounds() {
        assert_eq!(format_confidence(0.857), "0.86");
        assert_eq!(format_confidence(1.5), "1.00");
        assert_eq!(format_confidence(-0.1), "0.00");
    }
}
