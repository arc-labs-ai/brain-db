//! Trigger evaluation for the ENCODE path.
//!
//! An extractor's `trigger` (`on encode [where <cond>]`, `on demand`,
//! `periodic at ...`, `on schema_change`) decides whether it fires while
//! a memory is being encoded. Only `on encode` and `on encode where`
//! ever run on the encode path; the other kinds are driven by
//! out-of-band events (an explicit demand call, a cron tick, a schema
//! migration) the encode worker doesn't own, so they must NOT run here.
//!
//! The where-clause evaluator is deliberately small. It covers the two
//! documented field refs — `memory.text matches /re/` and
//! `memory.kind = <value>` — plus `and` / `or` composition and the `in`
//! membership test. Anything it can't resolve (an unknown field, an
//! unsupported operator, a bad regex) degrades to non-matching (the
//! extractor is skipped for that memory) with a `trace!` — never a
//! panic, and never a silent run over an out-of-scope memory.

use brain_core::{Memory, MemoryKind};
use brain_protocol::schema::ast::{ConditionExpr, ConditionOp, ConditionValue, TriggerExpr};

/// Outcome of asking "should this extractor run over this memory on the
/// ENCODE path?".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerDecision {
    /// The trigger fires for this memory — run the extractor.
    Run,
    /// An `on encode where` whose condition did not match this memory.
    /// The pipeline records a `SkippedFilter` for the (extractor, memory)
    /// pair.
    SkipFilter,
    /// Not an encode-path trigger at all (`on demand`, `periodic`,
    /// `on schema_change`). The extractor is inert during ENCODE
    /// regardless of the memory's content.
    NotOnEncode,
}

/// Decide whether `trigger` fires for `mem` on the ENCODE path.
#[must_use]
pub fn evaluate_trigger_on_encode(trigger: &TriggerExpr, mem: &Memory) -> TriggerDecision {
    match trigger {
        TriggerExpr::OnEncode => TriggerDecision::Run,
        TriggerExpr::OnEncodeWhere(cond) => {
            if eval_condition(cond, mem) {
                TriggerDecision::Run
            } else {
                TriggerDecision::SkipFilter
            }
        }
        TriggerExpr::OnDemand | TriggerExpr::OnSchemaChange | TriggerExpr::Periodic { .. } => {
            TriggerDecision::NotOnEncode
        }
    }
}

fn eval_condition(cond: &ConditionExpr, mem: &Memory) -> bool {
    match cond {
        ConditionExpr::And(a, b) => eval_condition(a, mem) && eval_condition(b, mem),
        ConditionExpr::Or(a, b) => eval_condition(a, mem) || eval_condition(b, mem),
        ConditionExpr::Matches { field, regex } => eval_matches(field, regex, mem),
        ConditionExpr::Atom { field, op, value } => eval_atom(field, *op, value, mem),
    }
}

/// Dotted field path as a single string, e.g. `["memory","text"]` →
/// `"memory.text"`. Both the fully-qualified (`memory.text`) and bare
/// (`text`) spellings are accepted so a schema author can write either.
fn field_path(field: &[String]) -> String {
    field.join(".")
}

fn eval_matches(field: &[String], regex: &str, mem: &Memory) -> bool {
    match field_path(field).as_str() {
        "memory.text" | "text" => {
            let text = mem.text.as_deref().unwrap_or("");
            // Compile on demand. A where-clause regex is authored once and
            // exercised rarely (the system extractor has none); a compile
            // failure is an operator config bug that must not run the
            // extractor over every memory, so it degrades to non-match.
            match regex::Regex::new(regex) {
                Ok(re) => re.is_match(text),
                Err(e) => {
                    tracing::trace!(
                        target: "brain_extractors::trigger",
                        regex = %regex,
                        error = %e,
                        "trigger `matches` regex failed to compile; treating as non-match",
                    );
                    false
                }
            }
        }
        other => {
            tracing::trace!(
                target: "brain_extractors::trigger",
                field = %other,
                "unsupported field in `matches` trigger; treating as non-match",
            );
            false
        }
    }
}

fn eval_atom(field: &[String], op: ConditionOp, value: &ConditionValue, mem: &Memory) -> bool {
    // Resolve the memory-side field to a comparable string. Only the two
    // documented refs are resolvable; anything else degrades to non-match.
    let actual: String = match field_path(field).as_str() {
        "memory.text" | "text" => mem.text.clone().unwrap_or_default(),
        "memory.kind" | "kind" => memory_kind_str(mem.kind).to_string(),
        other => {
            tracing::trace!(
                target: "brain_extractors::trigger",
                field = %other,
                "unsupported field in trigger condition; treating as non-match",
            );
            return false;
        }
    };

    match op {
        ConditionOp::Eq => value_as_str(value).is_some_and(|e| actual.eq_ignore_ascii_case(&e)),
        ConditionOp::Neq => value_as_str(value).is_some_and(|e| !actual.eq_ignore_ascii_case(&e)),
        ConditionOp::In => match value {
            ConditionValue::List(items) => items
                .iter()
                .filter_map(value_as_str)
                .any(|e| actual.eq_ignore_ascii_case(&e)),
            _ => false,
        },
        // Ordered comparisons over text/kind aren't meaningful for the
        // documented shapes; degrade to non-match rather than invent an
        // ordering.
        ConditionOp::Lt | ConditionOp::Lte | ConditionOp::Gt | ConditionOp::Gte => {
            tracing::trace!(
                target: "brain_extractors::trigger",
                op = ?op,
                "unsupported ordered operator in trigger condition; treating as non-match",
            );
            false
        }
    }
}

/// Render a scalar condition value as a string for case-insensitive
/// comparison. A bare list has no scalar form (only meaningful under
/// `in`), so it returns `None`.
fn value_as_str(value: &ConditionValue) -> Option<String> {
    match value {
        ConditionValue::Text(s) => Some(s.clone()),
        ConditionValue::Number(n) => Some(n.to_string()),
        ConditionValue::Bool(b) => Some(b.to_string()),
        ConditionValue::List(_) => None,
    }
}

fn memory_kind_str(kind: MemoryKind) -> &'static str {
    match kind {
        MemoryKind::Episodic => "episodic",
        MemoryKind::Semantic => "semantic",
        MemoryKind::Consolidated => "consolidated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brain_core::{SpaceId, SessionId, MemoryId, Salience};

    fn mem_with(text: &str, kind: MemoryKind) -> Memory {
        Memory {
            id: MemoryId::pack(0, 1, 0),
            space: SpaceId::new(),
            session_id: SessionId(0),
            kind,
            salience: Salience::default(),
            text: Some(text.to_string()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
            occurred_at_unix_nanos: None,
        }
    }

    fn atom(field: &[&str], op: ConditionOp, value: ConditionValue) -> ConditionExpr {
        ConditionExpr::Atom {
            field: field.iter().map(|s| (*s).to_string()).collect(),
            op,
            value,
        }
    }

    fn matches(field: &[&str], regex: &str) -> ConditionExpr {
        ConditionExpr::Matches {
            field: field.iter().map(|s| (*s).to_string()).collect(),
            regex: regex.to_string(),
        }
    }

    #[test]
    fn on_encode_always_runs() {
        let m = mem_with("anything", MemoryKind::Episodic);
        assert_eq!(
            evaluate_trigger_on_encode(&TriggerExpr::OnEncode, &m),
            TriggerDecision::Run
        );
    }

    #[test]
    fn non_encode_triggers_are_inert_on_encode() {
        let m = mem_with("anything", MemoryKind::Episodic);
        for t in [
            TriggerExpr::OnDemand,
            TriggerExpr::OnSchemaChange,
            TriggerExpr::Periodic {
                cron: "0 * * * *".into(),
            },
        ] {
            assert_eq!(
                evaluate_trigger_on_encode(&t, &m),
                TriggerDecision::NotOnEncode
            );
        }
    }

    #[test]
    fn where_text_matches_runs_on_hit_skips_on_miss() {
        let t = TriggerExpr::OnEncodeWhere(matches(&["memory", "text"], r"(?i)invoice"));
        assert_eq!(
            evaluate_trigger_on_encode(
                &t,
                &mem_with("Paid the INVOICE today", MemoryKind::Episodic)
            ),
            TriggerDecision::Run
        );
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("nothing relevant", MemoryKind::Episodic)),
            TriggerDecision::SkipFilter
        );
    }

    #[test]
    fn where_kind_eq_matches_case_insensitively() {
        let t = TriggerExpr::OnEncodeWhere(atom(
            &["memory", "kind"],
            ConditionOp::Eq,
            ConditionValue::Text("Semantic".into()),
        ));
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("x", MemoryKind::Semantic)),
            TriggerDecision::Run
        );
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("x", MemoryKind::Episodic)),
            TriggerDecision::SkipFilter
        );
    }

    #[test]
    fn where_and_or_compose() {
        let cond = ConditionExpr::And(
            Box::new(matches(&["memory", "text"], r"(?i)refund")),
            Box::new(atom(
                &["memory", "kind"],
                ConditionOp::Eq,
                ConditionValue::Text("episodic".into()),
            )),
        );
        let t = TriggerExpr::OnEncodeWhere(cond);
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("issued a Refund", MemoryKind::Episodic)),
            TriggerDecision::Run
        );
        // AND fails: right conjunct (kind) mismatches.
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("issued a Refund", MemoryKind::Semantic)),
            TriggerDecision::SkipFilter
        );

        let or = ConditionExpr::Or(
            Box::new(matches(&["memory", "text"], r"nomatchxyz")),
            Box::new(atom(
                &["memory", "kind"],
                ConditionOp::Eq,
                ConditionValue::Text("episodic".into()),
            )),
        );
        assert_eq!(
            evaluate_trigger_on_encode(
                &TriggerExpr::OnEncodeWhere(or),
                &mem_with("plain", MemoryKind::Episodic)
            ),
            TriggerDecision::Run
        );
    }

    #[test]
    fn where_kind_in_list() {
        let t = TriggerExpr::OnEncodeWhere(atom(
            &["memory", "kind"],
            ConditionOp::In,
            ConditionValue::List(vec![
                ConditionValue::Text("semantic".into()),
                ConditionValue::Text("consolidated".into()),
            ]),
        ));
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("x", MemoryKind::Consolidated)),
            TriggerDecision::Run
        );
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("x", MemoryKind::Episodic)),
            TriggerDecision::SkipFilter
        );
    }

    #[test]
    fn unresolved_field_skips() {
        let t = TriggerExpr::OnEncodeWhere(atom(
            &["entity", "type"],
            ConditionOp::Eq,
            ConditionValue::Text("Person".into()),
        ));
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("Alice", MemoryKind::Episodic)),
            TriggerDecision::SkipFilter
        );
    }

    #[test]
    fn bad_regex_degrades_to_skip() {
        let t = TriggerExpr::OnEncodeWhere(matches(&["memory", "text"], r"("));
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("anything", MemoryKind::Episodic)),
            TriggerDecision::SkipFilter
        );
    }

    #[test]
    fn unsupported_ordered_op_skips() {
        let t = TriggerExpr::OnEncodeWhere(atom(
            &["memory", "kind"],
            ConditionOp::Gt,
            ConditionValue::Text("episodic".into()),
        ));
        assert_eq!(
            evaluate_trigger_on_encode(&t, &mem_with("x", MemoryKind::Semantic)),
            TriggerDecision::SkipFilter
        );
    }
}
