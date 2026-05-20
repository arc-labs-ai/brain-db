//! RECALL result list renderer.
//!
//! The newtype wrap (`RecallResults(Vec<MemoryResult>)`) exists because
//! the orphan rule blocks `impl Render for Vec<...>`. The render form is
//! one stacked card per hit followed by an aggregate footer; the JSON
//! view is the bare array so downstream tools see the wire shape.

use std::io::{self, Write};

use brain_protocol::response::MemoryResult;
use serde_json::{json, Value};

use crate::render::{fmt_id, fmt_kind, fmt_short_id};
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
    use brain_protocol::request::MemoryKindWire;

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
        use brain_protocol::responses::types::RetrieverNameWire;
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
