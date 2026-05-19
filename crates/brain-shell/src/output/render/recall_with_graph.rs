//! RECALL renderer with knowledge enrichment per the flyctl stacked-card
//! pattern.
//!
//! ```text
//! m17  score=0.91 · 2m ago
//!   "Priya works at Acme Corp as a staff engineer"
//!   Entities:    Priya (Person) · Acme Corp (Org)
//!   Statements:  Priya works_at Acme Corp [0.95]
//!   Relations:   Priya --works_at→ Acme Corp
//! ```
//!
//! The memory id is wrapped in an OSC 8 hyperlink to `recall show m17`
//! so a click in a hyperlink-aware terminal opens the nested view.
//!
//! When the server hasn't populated the per-hit graph fields yet the
//! renderer falls back to the base recall card.

use std::io::{self, Write};

use brain_protocol::response::MemoryResult;
use serde_json::{json, Value};

use crate::output::table::{fmt_id, fmt_kind, fmt_short_id, middle_truncate};
use crate::output::term::{link, TermPolicy};
use crate::output::Render;

/// Per-hit knowledge enrichment populated when the caller passes
/// `--include-graph`. Empty vectors are rendered as omitted lines so
/// the card stays compact for unenriched memories.
#[derive(Debug, Default, Clone)]
pub struct GraphEnrichment {
    pub entities: Vec<EnrichedEntity>,
    pub statements: Vec<EnrichedStatement>,
    pub relations: Vec<EnrichedRelation>,
}

#[derive(Debug, Clone)]
pub struct EnrichedEntity {
    pub id: String,
    pub name: String,
    pub type_qname: String,
}

#[derive(Debug, Clone)]
pub struct EnrichedStatement {
    pub id: String,
    pub subject_name: String,
    pub predicate: String,
    pub object_label: String,
    pub confidence: f32,
}

#[derive(Debug, Clone)]
pub struct EnrichedRelation {
    pub from_name: String,
    pub predicate: String,
    pub to_name: String,
}

/// Renderer wrapping the recall hits + per-hit enrichment.
pub struct RecallWithGraph {
    pub hits: Vec<MemoryResult>,
    pub graphs: Vec<GraphEnrichment>,
}

impl Render for RecallWithGraph {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        if self.hits.is_empty() {
            return writeln!(w, "(no results)");
        }
        let policy = TermPolicy::plain();
        let body_width = policy.width.saturating_sub(4);
        for (idx, hit) in self.hits.iter().enumerate() {
            let short = fmt_short_id(hit.memory_id);
            let id_cell = link(policy, &short, &format!("brain://recall/{short}"));
            writeln!(
                w,
                "{id_cell}  score={:.2} · {}",
                hit.similarity_score,
                fmt_kind(hit.kind),
            )?;
            let text = if hit.text.is_empty() {
                "(text not fetched — re-run with --include-text)".to_string()
            } else {
                format!("\"{}\"", middle_truncate(&hit.text, body_width))
            };
            writeln!(w, "  {text}")?;

            if let Some(graph) = self.graphs.get(idx) {
                if !graph.entities.is_empty() {
                    let names: Vec<String> = graph
                        .entities
                        .iter()
                        .map(|e| format!("{} ({})", e.name, e.type_qname))
                        .collect();
                    writeln!(w, "  Entities:    {}", names.join(" · "))?;
                }
                if !graph.statements.is_empty() {
                    for s in &graph.statements {
                        writeln!(
                            w,
                            "  Statements:  {} {} {} [{:.2}]",
                            s.subject_name, s.predicate, s.object_label, s.confidence
                        )?;
                    }
                }
                if !graph.relations.is_empty() {
                    for r in &graph.relations {
                        writeln!(
                            w,
                            "  Relations:   {} --{}→ {}",
                            r.from_name, r.predicate, r.to_name
                        )?;
                    }
                }
            }
            if idx + 1 < self.hits.len() {
                writeln!(w)?;
            }
        }
        writeln!(w)?;
        writeln!(w, "{} results", self.hits.len())
    }

    fn to_json_value(&self) -> Value {
        let items: Vec<Value> = self
            .hits
            .iter()
            .enumerate()
            .map(|(i, r)| {
                let graph = self.graphs.get(i).cloned().unwrap_or_default();
                json!({
                    "memory_id": fmt_id(r.memory_id),
                    "similarity_score": r.similarity_score,
                    "kind": fmt_kind(r.kind),
                    "text": r.text,
                    "entities": graph.entities.iter().map(|e| json!({
                        "id": e.id,
                        "name": e.name,
                        "type": e.type_qname,
                    })).collect::<Vec<_>>(),
                    "statements": graph.statements.iter().map(|s| json!({
                        "id": s.id,
                        "subject_name": s.subject_name,
                        "predicate": s.predicate,
                        "object_label": s.object_label,
                        "confidence": s.confidence,
                    })).collect::<Vec<_>>(),
                    "relations": graph.relations.iter().map(|r| json!({
                        "from_name": r.from_name,
                        "predicate": r.predicate,
                        "to_name": r.to_name,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        Value::Array(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::MemoryId;
    use brain_protocol::request::MemoryKindWire;

    fn make_hit(text: &str) -> MemoryResult {
        MemoryResult {
            memory_id: MemoryId::pack(2, 17, 1).raw(),
            text: text.into(),
            similarity_score: 0.91,
            confidence: 0.91,
            salience: 0.5,
            kind: MemoryKindWire::Episodic,
            context_id: 0,
            created_at_unix_nanos: 0,
            last_accessed_at_unix_nanos: 0,
            vector_offset: 0,
            vector_dim: 0,
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

    #[test]
    fn renders_hit_with_graph() {
        let r = RecallWithGraph {
            hits: vec![make_hit("Priya works at Acme Corp")],
            graphs: vec![GraphEnrichment {
                entities: vec![EnrichedEntity {
                    id: "e1".into(),
                    name: "Priya".into(),
                    type_qname: "Person".into(),
                }],
                statements: vec![EnrichedStatement {
                    id: "s1".into(),
                    subject_name: "Priya".into(),
                    predicate: "works_at".into(),
                    object_label: "Acme".into(),
                    confidence: 0.95,
                }],
                relations: vec![],
            }],
        };
        let mut buf = Vec::new();
        r.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("s2/m17/v1"));
        assert!(s.contains("Priya works at Acme"));
        assert!(s.contains("Entities:") && s.contains("Priya (Person)"));
        assert!(s.contains("Statements:") && s.contains("works_at"));
    }

    #[test]
    fn omits_empty_graph_sections() {
        let r = RecallWithGraph {
            hits: vec![make_hit("plain memory")],
            graphs: vec![GraphEnrichment::default()],
        };
        let mut buf = Vec::new();
        r.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("Entities:"));
        assert!(!s.contains("Statements:"));
    }

    #[test]
    fn empty_hits_yields_no_results_marker() {
        let r = RecallWithGraph {
            hits: vec![],
            graphs: vec![],
        };
        let mut buf = Vec::new();
        r.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("(no results)"));
    }
}
