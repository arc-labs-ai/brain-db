//! REASON response renderer — one row per inference step.

use std::io::{self, Write};

use brain_protocol::envelope::response::InferenceStep;
use comfy_table::Cell;
use serde_json::{json, Value};

use crate::render::fmt_id;
use crate::table::{build_table, confidence_cell};
use crate::theme::Token;
use crate::{Render, RenderCtx};

/// Newtype around `Vec<InferenceStep>` so we can implement [`Render`].
pub struct ReasonSteps(pub Vec<InferenceStep>);

impl Render for ReasonSteps {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let policy = ctx.policy;
        let theme = &ctx.theme;
        let mut table = build_table(policy);
        table.set_header(vec![
            Cell::new(theme.paint(Token::Label, "step", policy)),
            Cell::new(theme.paint(Token::Label, "kind", policy)),
            Cell::new(theme.paint(Token::Label, "conf", policy)),
            Cell::new(theme.paint(Token::Label, "supports", policy)),
            Cell::new(theme.paint(Token::Label, "contradicts", policy)),
            Cell::new(theme.paint(Token::Label, "claim", policy)),
        ]);
        for s in &self.0 {
            table.add_row(vec![
                Cell::new(s.step_index),
                Cell::new(format!("{:?}", s.inference_kind)),
                confidence_cell(theme, policy, s.confidence),
                Cell::new(s.supporting_memories.len()),
                Cell::new(s.contradicting_memories.len()),
                Cell::new(&s.claim),
            ]);
        }
        writeln!(w, "{table}")
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let items: Vec<Value> = self
            .0
            .iter()
            .map(|s| {
                json!({
                    "step_index": s.step_index,
                    "inference_kind": format!("{:?}", s.inference_kind),
                    "claim": s.claim,
                    "confidence": s.confidence,
                    "supporting_memories": s.supporting_memories.iter().map(|m| fmt_id(*m)).collect::<Vec<_>>(),
                    "contradicting_memories": s.contradicting_memories.iter().map(|m| fmt_id(*m)).collect::<Vec<_>>(),
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
    use brain_protocol::shared::enums::InferenceKind;

    fn ctx() -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: OutputFormat::Table,
        }
    }

    #[test]
    fn table_renders_steps() {
        let r = ReasonSteps(vec![InferenceStep {
            step_index: 0,
            claim: "alice knows bob".into(),
            supporting_memories: vec![0x1, 0x2],
            contradicting_memories: vec![],
            confidence: 0.8,
            inference_kind: InferenceKind::EvidenceAccumulation,
        }]);
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("alice knows bob"));
        assert!(s.contains("EvidenceAccumulation"));
    }
}
