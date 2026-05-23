//! RECALL result list renderer.
//!
//! The newtype wrap (`RecallResults(Vec<MemoryResult>)`) exists because
//! the orphan rule blocks `impl Render for Vec<...>`. The render form is
//! one stacked card per hit followed by an aggregate footer; the JSON
//! view is the bare array so downstream tools see the wire shape.

use std::io::{self, Write};

use brain_protocol::envelope::response::MemoryResult;
use serde_json::{json, Value};

use crate::render::{fmt_hex_16, fmt_id, fmt_kind, fmt_short_id};
use crate::table::middle_truncate;
use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Newtype around `Vec<MemoryResult>` so we can implement [`Render`]
/// without running into the orphan rule.
pub struct RecallResults(pub Vec<MemoryResult>);

impl Render for RecallResults {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let results = &self.0;
        if results.is_empty() {
            return writeln!(w, "(no results)");
        }
        let policy = ctx.policy;
        let theme = &ctx.theme;
        for (idx, r) in results.iter().enumerate() {
            let kind_str = if r.consolidated_at_unix_nanos.is_some() {
                format!("{}†", fmt_kind(r.kind))
            } else {
                fmt_kind(r.kind).to_string()
            };
            let salience = if (r.salience - r.salience_initial).abs() < 0.001 {
                format!("sal={:.3}", r.salience)
            } else {
                let arrow = if r.salience < r.salience_initial {
                    "↓"
                } else {
                    "↑"
                };
                format!("sal={:.3}{arrow}{:.3}", r.salience, r.salience_initial)
            };
            let short = fmt_short_id(r.memory_id);
            let id_painted = theme.paint(Token::MemoryId, &short, policy).into_owned();
            let score_painted = {
                let s = format!("score={:.4}", r.similarity_score);
                theme.paint(Token::Score, &s, policy).into_owned()
            };
            let mut meta: Vec<String> = vec![
                id_painted,
                kind_str,
                format!("ctx={}", r.context_id),
                salience,
                score_painted,
            ];
            if r.access_count > 0 {
                meta.push(format!("acc={}", r.access_count));
            }
            if r.edges_in_count > 0 || r.edges_out_count > 0 {
                meta.push(format!(
                    "edges={}in/{}out",
                    r.edges_in_count, r.edges_out_count
                ));
            }
            // Hybrid hits carry a non-empty contributing_retrievers
            // list (substrate path leaves it empty). Surfacing the
            // count flags single-retriever hits — they're typically
            // weak signal that only one of {semantic, lexical, graph}
            // ranked the row, so the user shouldn't read fused-score
            // ordering as authoritative for them.
            if !r.contributing_retrievers.is_empty() {
                meta.push(format!("retrievers={}", r.contributing_retrievers.len()));
            }
            writeln!(w, "#{}  {}", idx + 1, meta.join("  "))?;
            if r.text.is_empty() {
                let hint = theme.paint(
                    Token::Muted,
                    "(text not fetched — re-run with --include-text)",
                    policy,
                );
                writeln!(w, "    {hint}")?;
            } else {
                // Reserve indent + margin so long memory text wraps cleanly
                // to the detected terminal width.
                let max = policy.width.saturating_sub(6);
                writeln!(w, "    {}", middle_truncate(&r.text, max))?;
            }
            // Per-hit outgoing edge list — only when --include-edges
            // populated `r.edges`. `Some(vec![])` means "we asked, got
            // none"; render the muted "no outgoing edges" line so the
            // user can tell apart "didn't ask" from "asked, empty".
            if let Some(edges) = &r.edges {
                if edges.is_empty() {
                    let hint = theme.paint(Token::Muted, "    (no outgoing edges)", policy);
                    writeln!(w, "{hint}")?;
                } else {
                    for e in edges {
                        let tgt = fmt_short_id(e.target);
                        let kind_label = format!("{:?}", e.kind);
                        let arrow = theme.paint(Token::Muted, "→", policy);
                        let line =
                            format!("    {arrow} {kind_label}  {tgt}  weight={:.3}", e.weight);
                        writeln!(w, "{line}")?;
                    }
                }
            }
            // Per-hit knowledge-layer enrichment — only when
            // --include-graph populated `r.graph`. `None` distinguishes
            // "didn't ask" / "memory wasn't extracted" from "asked, got
            // empty lists." Empty sub-vectors render as muted "(none)"
            // lines so a reader can tell the extraction ran but found
            // nothing of that kind.
            if let Some(graph) = &r.graph {
                if !graph.entities.is_empty() {
                    let label = theme.paint(Token::Label, "    Entities:", policy);
                    let names: Vec<String> = graph
                        .entities
                        .iter()
                        .map(|e| format!("{} ({})", e.name, e.type_qname))
                        .collect();
                    writeln!(w, "{label} {}", names.join(" · "))?;
                }
                if !graph.statements.is_empty() {
                    let label = theme.paint(Token::Label, "    Statements:", policy);
                    writeln!(w, "{label}")?;
                    for s in &graph.statements {
                        let arrow = theme.paint(Token::Muted, "→", policy);
                        writeln!(
                            w,
                            "      {arrow} {} {} {} [{:.2}]",
                            s.subject_name, s.predicate, s.object_label, s.confidence,
                        )?;
                    }
                }
                if !graph.relations.is_empty() {
                    let label = theme.paint(Token::Label, "    Relations:", policy);
                    writeln!(w, "{label}")?;
                    for rel in &graph.relations {
                        let arrow = theme.paint(Token::Muted, "→", policy);
                        writeln!(
                            w,
                            "      {} --{}{} {}",
                            rel.from_name, rel.predicate, arrow, rel.to_name,
                        )?;
                    }
                }
                if graph.entities.is_empty()
                    && graph.statements.is_empty()
                    && graph.relations.is_empty()
                {
                    let hint = theme.paint(
                        Token::Muted,
                        "    (no knowledge enrichment — extractor produced no entities/statements/relations)",
                        policy,
                    );
                    writeln!(w, "{hint}")?;
                }
            }
            if idx + 1 < results.len() {
                writeln!(w)?;
            }
        }
        writeln!(w)?;
        let n = results.len();
        let score_spread = {
            let mut min = f32::INFINITY;
            let mut max = f32::NEG_INFINITY;
            for r in results {
                if r.similarity_score < min {
                    min = r.similarity_score;
                }
                if r.similarity_score > max {
                    max = r.similarity_score;
                }
            }
            max - min
        };
        if n >= 2 && score_spread < 0.001 {
            let warn = theme.paint(
                Token::Warn,
                "scores tightly clustered (Δ<0.001) — ranking may not be meaningful",
                policy,
            );
            writeln!(w, "{n} results  ·  {warn}")
        } else {
            writeln!(w, "{n} results")
        }
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let items: Vec<Value> = self
            .0
            .iter()
            .map(|r| {
                json!({
                    "memory_id": fmt_id(r.memory_id),
                    "similarity_score": r.similarity_score,
                    "confidence": r.confidence,
                    "salience": r.salience,
                    "salience_initial": r.salience_initial,
                    "access_count": r.access_count,
                    "lsn": r.lsn,
                    "flags": r.flags,
                    "kind": fmt_kind(r.kind),
                    "context_id": r.context_id,
                    "created_at_unix_nanos": r.created_at_unix_nanos,
                    "last_accessed_at_unix_nanos": r.last_accessed_at_unix_nanos,
                    "consolidated_at_unix_nanos": r.consolidated_at_unix_nanos,
                    "edges_out_count": r.edges_out_count,
                    "edges_in_count": r.edges_in_count,
                    "edges": r.edges.as_ref().map(|es| es.iter().map(|e| json!({
                        "target": fmt_id(e.target),
                        "kind": format!("{:?}", e.kind),
                        "weight": e.weight,
                    })).collect::<Vec<_>>()),
                    "graph": r.graph.as_ref().map(|g| json!({
                        "entities": g.entities.iter().map(|e| json!({
                            "id": fmt_hex_16(&e.id),
                            "name": e.name,
                            "type_qname": e.type_qname,
                        })).collect::<Vec<_>>(),
                        "statements": g.statements.iter().map(|s| json!({
                            "id": fmt_hex_16(&s.id),
                            "subject_name": s.subject_name,
                            "predicate": s.predicate,
                            "object_label": s.object_label,
                            "confidence": s.confidence,
                        })).collect::<Vec<_>>(),
                        "relations": g.relations.iter().map(|rel| json!({
                            "from_name": rel.from_name,
                            "predicate": rel.predicate,
                            "to_name": rel.to_name,
                        })).collect::<Vec<_>>(),
                    })),
                    "fused_score": r.fused_score,
                    "text": r.text,
                })
            })
            .collect();
        Value::Array(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;
    use brain_core::MemoryId;
    use brain_protocol::envelope::request::MemoryKindWire;

    fn make_hit(text: &str, score: f32) -> MemoryResult {
        MemoryResult {
            memory_id: MemoryId::pack(2, 17, 1).raw(),
            text: text.into(),
            similarity_score: score,
            confidence: score,
            salience: 0.5,
            kind: MemoryKindWire::Episodic,
            context_id: 0,
            created_at_unix_nanos: 0,
            last_accessed_at_unix_nanos: 0,
            edges: None,
            contributing_retrievers: Vec::new(),
            fused_score: 0.0,
            salience_initial: 0.5,
            access_count: 0,
            lsn: 0,
            flags: 0,
            consolidated_at_unix_nanos: None,
            edges_out_count: 0,
            edges_in_count: 0,
            graph: None,
        }
    }

    fn ctx() -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: OutputFormat::Table,
        }
    }

    #[test]
    fn empty_renders_no_results_marker() {
        let r = RecallResults(vec![]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        assert!(String::from_utf8(buf).unwrap().contains("(no results)"));
    }

    #[test]
    fn renders_single_hit() {
        let r = RecallResults(vec![make_hit("the quick brown fox", 0.91)]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("s2/m17/v1"));
        assert!(s.contains("score=0.9100"));
        assert!(s.contains("the quick brown fox"));
        assert!(s.contains("1 results"));
    }

    #[test]
    fn flags_clustered_scores() {
        // Two hits with identical scores → footer warns about ranking.
        let r = RecallResults(vec![make_hit("a", 0.5), make_hit("b", 0.5)]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("tightly clustered"), "missing cluster warn: {s}");
    }

    #[test]
    fn narrow_width_truncates_text() {
        let mut policy = TermPolicy::plain();
        policy.width = 40;
        let ctx = RenderCtx {
            policy,
            theme: Theme::default(),
            format: OutputFormat::Table,
        };
        let long = "x".repeat(200);
        let r = RecallResults(vec![make_hit(&long, 0.9)]);
        let mut buf = Vec::new();
        r.render_table(&ctx, &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains('…'), "should middle-truncate: {s}");
    }

    #[test]
    fn substrate_hit_omits_retrievers_column() {
        // Empty contributing_retrievers (the substrate-path default)
        // → no retrievers=N column. Keeps the row tight when hybrid
        // metadata isn't available.
        let r = RecallResults(vec![make_hit("substrate hit", 0.9)]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains("retrievers="),
            "substrate row should not show retrievers count: {s}"
        );
    }

    #[test]
    fn hybrid_hit_shows_retrievers_count() {
        // Non-empty contributing_retrievers → retrievers=N column.
        // A single-retriever hit (count=1) is the signal that a row
        // only matched one of the routed retrievers.
        use brain_protocol::RetrieverNameWire;
        let mut hit = make_hit("hybrid hit", 0.9);
        hit.contributing_retrievers = vec![RetrieverNameWire::Semantic];
        let r = RecallResults(vec![hit]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("retrievers=1"), "expected retrievers=1: {s}");

        // Multi-retriever consensus.
        let mut hit = make_hit("strong hit", 0.9);
        hit.contributing_retrievers = vec![RetrieverNameWire::Semantic, RetrieverNameWire::Lexical];
        let r = RecallResults(vec![hit]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("retrievers=2"), "expected retrievers=2: {s}");
    }

    #[test]
    fn json_view_is_array_of_objects() {
        let r = RecallResults(vec![make_hit("hi", 0.91)]);
        let v = r.render_json(&ctx());
        let arr = v.as_array().expect("array");
        assert_eq!(arr.len(), 1);
        let s = arr[0]["similarity_score"].as_f64().unwrap();
        assert!((s - 0.91).abs() < 1e-4);
        assert_eq!(arr[0]["kind"], "episodic");
    }
}
