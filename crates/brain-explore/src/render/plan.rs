//! PLAN response renderer — steps plus an optional final status.
//!
//! Wrapped in [`PlanSteps`] so the renderer can attach a status footer
//! that explains *why* the path is shorter than expected (no-path,
//! budget exhausted, cancelled). Without that footer the operator
//! sees a one-row table and can't tell "1 step found" from
//! "started+gave-up."

use std::io::{self, Write};

use brain_protocol::envelope::response::{PlanStatus, PlanStep};
use comfy_table::{Cell, Row};
use serde_json::{json, Value};

use crate::render::{fmt_id, fmt_short_id};
use crate::table::{build_table, confidence_cell};
use crate::theme::Token;
use crate::{Render, RenderCtx};

pub struct PlanSteps {
    pub steps: Vec<PlanStep>,
    pub status: Option<PlanStatus>,
}

impl Render for PlanSteps {
    fn render_table(&self, ctx: &RenderCtx, w: &mut dyn Write) -> io::Result<()> {
        let policy = ctx.policy;
        let theme = &ctx.theme;
        let mut table = build_table(policy);
        table.set_header(vec![
            Cell::new(theme.paint(Token::Label, "step", policy)),
            Cell::new(theme.paint(Token::Label, "id", policy)),
            Cell::new(theme.paint(Token::Label, "transition", policy)),
            Cell::new(theme.paint(Token::Label, "conf", policy)),
            Cell::new(theme.paint(Token::Label, "remaining", policy)),
            Cell::new(theme.paint(Token::Label, "text", policy)),
        ]);
        for s in &self.steps {
            let mut row = Row::new();
            row.add_cell(Cell::new(s.step_index));
            row.add_cell(Cell::new(theme.paint(
                Token::MemoryId,
                &fmt_short_id(s.memory_id),
                policy,
            )));
            row.add_cell(Cell::new(format!("{:?}", s.transition_kind)));
            row.add_cell(confidence_cell(theme, policy, s.confidence));
            row.add_cell(Cell::new(format!("{:.4}", s.estimated_distance_to_goal)));
            row.add_cell(Cell::new(&s.text));
            table.add_row(row);
        }
        writeln!(w, "{table}")?;
        if let Some(footer) = plan_status_footer(self.status, self.steps.len()) {
            let painted = theme.paint(Token::Muted, &footer, policy);
            writeln!(w, "{painted}")?;
        }
        Ok(())
    }

    fn render_json(&self, _ctx: &RenderCtx) -> Value {
        let items: Vec<Value> = self
            .steps
            .iter()
            .map(|s| {
                json!({
                    "step_index": s.step_index,
                    "memory_id": fmt_id(s.memory_id),
                    "transition_kind": format!("{:?}", s.transition_kind),
                    "confidence": s.confidence,
                    "estimated_distance_to_goal": s.estimated_distance_to_goal,
                    "text": s.text,
                })
            })
            .collect();
        json!({
            "steps": Value::Array(items),
            "status": self.status.map(fmt_plan_status_json),
        })
    }
}

fn fmt_plan_status_json(s: PlanStatus) -> Value {
    Value::String(
        match s {
            PlanStatus::GoalReached => "GoalReached",
            PlanStatus::BudgetExhausted => "BudgetExhausted",
            PlanStatus::NoPathFound => "NoPathFound",
            PlanStatus::Cancelled => "Cancelled",
        }
        .to_owned(),
    )
}

/// Build the human-facing explanation for a non-trivial plan outcome.
///
/// Returns `None` when the plan succeeded or when no status was attached —
/// success doesn't need narration and an unset status means the caller
/// hasn't told us anything to explain.
fn plan_status_footer(status: Option<PlanStatus>, n_steps: usize) -> Option<String> {
    let status = status?;
    match status {
        PlanStatus::GoalReached => None,
        PlanStatus::NoPathFound => Some(format!(
            "(NoPathFound — no path between the start and goal within the index{})",
            if n_steps <= 1 {
                "; only the start endpoint surfaced"
            } else {
                ""
            },
        )),
        PlanStatus::BudgetExhausted => {
            Some("(BudgetExhausted — try a larger --max-steps or --max-wall-time-ms)".to_owned())
        }
        PlanStatus::Cancelled => Some("(Cancelled)".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::OutputFormat;
    use crate::theme::Theme;
    use crate::TermPolicy;
    use brain_protocol::shared::enums::TransitionKind;

    fn ctx() -> RenderCtx {
        RenderCtx {
            policy: TermPolicy::plain(),
            theme: Theme::default(),
            format: OutputFormat::Table,
        }
    }

    fn step(idx: u32) -> PlanStep {
        PlanStep {
            step_index: idx,
            memory_id: 0x100,
            text: format!("step {idx}"),
            transition_kind: TransitionKind::Initial,
            confidence: 0.5,
            estimated_distance_to_goal: 0.2,
        }
    }

    #[test]
    fn table_renders_steps() {
        let r = PlanSteps {
            steps: vec![step(0), step(1)],
            status: Some(PlanStatus::GoalReached),
        };
        let mut buf = Vec::new();
        r.render_table(&ctx(), &mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("step 0"));
        assert!(s.contains("step 1"));
    }

    #[test]
    fn footer_silent_on_goal_reached() {
        assert!(plan_status_footer(Some(PlanStatus::GoalReached), 3).is_none());
        assert!(plan_status_footer(None, 3).is_none());
    }

    #[test]
    fn footer_explains_no_path_found_for_single_step() {
        let f = plan_status_footer(Some(PlanStatus::NoPathFound), 1).expect("footer");
        assert!(f.contains("only the start endpoint surfaced"), "{f}");
    }

    #[test]
    fn footer_explains_budget_exhausted() {
        let f = plan_status_footer(Some(PlanStatus::BudgetExhausted), 0).expect("footer");
        assert!(f.contains("--max-steps") || f.contains("--max-wall-time-ms"));
    }
}
