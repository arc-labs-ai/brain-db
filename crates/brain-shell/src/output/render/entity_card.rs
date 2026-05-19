//! Flyctl-style stacked card for `entity show`.
//!
//! Sections (in order, optional): Identity · Aliases · Statements ·
//! Mentioned-in · Relations. Long lines are middle-truncated to the
//! terminal width. Entity ids in the Relations section are wrapped in
//! OSC 8 hyperlinks to a nested `entity show` query so a click in iTerm
//! or kitty navigates the graph.

use std::io::{self, Write};

use serde_json::{json, Value};

use crate::output::table::middle_truncate;
use crate::output::term::{link, TermPolicy};
use crate::output::Render;

/// Renderer for a single entity's full record.
pub struct EntityCard {
    pub id: String,
    pub canonical_name: String,
    pub type_qname: String,
    pub aliases: Vec<String>,
    pub statements: Vec<StatementSummary>,
    pub mentioned_in: Vec<MemorySummary>,
    pub relations_out: Vec<RelationSummary>,
    pub relations_in: Vec<RelationSummary>,
}

pub struct StatementSummary {
    pub id: String,
    pub kind: String,
    pub predicate: String,
    pub object: String,
    pub confidence: f32,
}

pub struct MemorySummary {
    pub short_id: String,
    pub text: String,
}

pub struct RelationSummary {
    pub other_id: String,
    pub other_name: String,
    pub predicate: String,
}

impl Render for EntityCard {
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        // Plain policy at render time; the dispatcher upgrades by
        // passing a different policy via wrapper APIs in a later
        // iteration. For now the OSC 8 link helper still emits when
        // `policy.hyperlinks` is on.
        let policy = TermPolicy::plain();
        let body_width = policy.width.saturating_sub(2);

        writeln!(w, "{}  ({})", self.canonical_name, self.type_qname)?;
        writeln!(w, "  id = {}", self.id)?;
        writeln!(w)?;

        if !self.aliases.is_empty() {
            writeln!(w, "Aliases")?;
            for a in &self.aliases {
                writeln!(w, "  · {}", a)?;
            }
            writeln!(w)?;
        }

        if !self.statements.is_empty() {
            writeln!(w, "Statements")?;
            for s in &self.statements {
                let line = format!(
                    "[{}] {} {} (conf {:.2})  id={}",
                    s.kind, s.predicate, s.object, s.confidence, s.id,
                );
                writeln!(w, "  · {}", middle_truncate(&line, body_width))?;
            }
            writeln!(w)?;
        }

        if !self.mentioned_in.is_empty() {
            writeln!(w, "Mentioned in")?;
            for m in &self.mentioned_in {
                let memory_link = link(
                    policy,
                    &m.short_id,
                    &format!("brain://recall/{}", m.short_id),
                );
                let text = middle_truncate(&m.text, body_width.saturating_sub(20));
                writeln!(w, "  · {memory_link}  {text}")?;
            }
            writeln!(w)?;
        }

        if !self.relations_out.is_empty() {
            writeln!(w, "Relations (out)")?;
            for r in &self.relations_out {
                let other_link = link(
                    policy,
                    &r.other_name,
                    &format!("brain://entity/{}", r.other_id),
                );
                writeln!(w, "  · --[{}]--> {other_link}", r.predicate)?;
            }
            writeln!(w)?;
        }

        if !self.relations_in.is_empty() {
            writeln!(w, "Relations (in)")?;
            for r in &self.relations_in {
                let other_link = link(
                    policy,
                    &r.other_name,
                    &format!("brain://entity/{}", r.other_id),
                );
                writeln!(w, "  · {other_link} --[{}]--> ·", r.predicate)?;
            }
            writeln!(w)?;
        }

        Ok(())
    }

    fn to_json_value(&self) -> Value {
        json!({
            "id": self.id,
            "canonical_name": self.canonical_name,
            "type": self.type_qname,
            "aliases": self.aliases,
            "statements": self.statements.iter().map(|s| json!({
                "id": s.id,
                "kind": s.kind,
                "predicate": s.predicate,
                "object": s.object,
                "confidence": s.confidence,
            })).collect::<Vec<_>>(),
            "mentioned_in": self.mentioned_in.iter().map(|m| json!({
                "memory_id": m.short_id,
                "text": m.text,
            })).collect::<Vec<_>>(),
            "relations_out": self.relations_out.iter().map(|r| json!({
                "other_id": r.other_id,
                "other_name": r.other_name,
                "predicate": r.predicate,
            })).collect::<Vec<_>>(),
            "relations_in": self.relations_in.iter().map(|r| json!({
                "other_id": r.other_id,
                "other_name": r.other_name,
                "predicate": r.predicate,
            })).collect::<Vec<_>>(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_card() -> EntityCard {
        EntityCard {
            id: "ent_a1b2".into(),
            canonical_name: "Priya".into(),
            type_qname: "Person".into(),
            aliases: vec!["P.".into()],
            statements: vec![StatementSummary {
                id: "stmt_1".into(),
                kind: "Fact".into(),
                predicate: "works_at".into(),
                object: "Acme Corp".into(),
                confidence: 0.95,
            }],
            mentioned_in: vec![MemorySummary {
                short_id: "s2/m17/v1".into(),
                text: "Priya works at Acme Corp as a staff engineer".into(),
            }],
            relations_out: vec![RelationSummary {
                other_id: "ent_xy".into(),
                other_name: "Acme Corp".into(),
                predicate: "works_at".into(),
            }],
            relations_in: vec![],
        }
    }

    fn strip_osc8(s: &str) -> String {
        // Drop `\x1b]8;;...\x1b\\` markers so golden assertions don't
        // depend on whether hyperlinks were on at render time.
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\x1b' && chars.peek() == Some(&']') {
                while let Some(next) = chars.next() {
                    if next == '\\' {
                        break;
                    }
                }
                continue;
            }
            out.push(c);
        }
        out
    }

    #[test]
    fn renders_all_sections() {
        let card = sample_card();
        let mut buf = Vec::new();
        card.render_table(&mut buf).unwrap();
        let s = strip_osc8(&String::from_utf8(buf).unwrap());
        assert!(s.contains("Priya"), "header missing: {s}");
        assert!(s.contains("Aliases"), "aliases header missing: {s}");
        assert!(
            s.contains("Statements") && s.contains("works_at"),
            "statements section missing: {s}",
        );
        assert!(
            s.contains("Mentioned in") && s.contains("s2/m17/v1"),
            "mentions section missing: {s}",
        );
        assert!(
            s.contains("Relations (out)"),
            "relations (out) header missing: {s}",
        );
    }

    #[test]
    fn omits_empty_sections() {
        let mut card = sample_card();
        card.aliases.clear();
        card.mentioned_in.clear();
        let mut buf = Vec::new();
        card.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(!s.contains("Aliases"));
        assert!(!s.contains("Mentioned in"));
    }

    #[test]
    fn json_value_contains_all_fields() {
        let card = sample_card();
        let v = card.to_json_value();
        assert_eq!(v["canonical_name"], "Priya");
        assert_eq!(v["type"], "Person");
        assert_eq!(v["statements"][0]["predicate"], "works_at");
    }
}
