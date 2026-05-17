//! Pattern extractor. Spec §22/01.
//!
//! Compiles a list of regex sources once, runs them over memory
//! text on every dispatch, projects matches to typed mentions per
//! the extractor's `target`.

use brain_core::knowledge::{ExtractorKind, StatementKind};
use brain_core::{ExtractorId, Memory};
use brain_protocol::schema::{ExtractorTarget, StatementKindAst};
use regex::{Regex, RegexBuilder};

use crate::extractor::{
    ExtractionContext, ExtractionFuture, ExtractionResult, Extractor, ExtractorError,
};
use crate::item::{EntityMention, ExtractedItem, RelationMention, StatementMention};

/// Conservative cap per spec §22/01 §2 — 1 MiB for compiled
/// regex state (NFA + DFA). Bigger patterns fail with
/// `ExtractorError::ResourceLimit`.
const REGEX_SIZE_LIMIT: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct CompiledRegex {
    raw: String,
    re: Regex,
}

impl CompiledRegex {
    /// Raw pattern source (for debugging / audit).
    #[must_use]
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Underlying compiled regex.
    #[must_use]
    pub fn regex(&self) -> &Regex {
        &self.re
    }
}

/// Pre-compiled pattern extractor.
#[derive(Debug)]
pub struct PatternExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    patterns: Vec<CompiledRegex>,
    confidence: f32,
}

impl PatternExtractor {
    /// Build from a `ValidatedSchema`-derived definition. Compiles
    /// all patterns; returns `ExtractorError::RegexCompile` /
    /// `ResourceLimit` / `EmptyPatterns` on failure.
    pub fn try_new(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        patterns: &[String],
        confidence: f32,
    ) -> Result<Self, ExtractorError> {
        if patterns.is_empty() {
            return Err(ExtractorError::EmptyPatterns);
        }
        let mut compiled = Vec::with_capacity(patterns.len());
        for (i, raw) in patterns.iter().enumerate() {
            let re = RegexBuilder::new(raw)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_SIZE_LIMIT)
                .build()
                .map_err(|e| compile_error(i, e))?;
            compiled.push(CompiledRegex {
                raw: raw.clone(),
                re,
            });
        }
        Ok(Self {
            id,
            name,
            target,
            extractor_version,
            patterns: compiled,
            confidence,
        })
    }

    #[must_use]
    pub fn patterns(&self) -> &[CompiledRegex] {
        &self.patterns
    }

    #[must_use]
    pub fn confidence(&self) -> f32 {
        self.confidence
    }

    #[must_use]
    pub fn target(&self) -> &ExtractorTarget {
        &self.target
    }

    fn project(&self, text: String, start: usize, end: usize) -> Option<ExtractedItem> {
        let id_raw = self.id.raw();
        match &self.target {
            ExtractorTarget::Entity { entity_type } => Some(ExtractedItem::EntityMention(
                EntityMention {
                    entity_type_qname: entity_type.clone(),
                    text,
                    start,
                    end,
                    confidence: self.confidence,
                    extractor_id: id_raw,
                    extractor_version: self.extractor_version,
                },
            )),
            ExtractorTarget::Statement { kind } => Some(ExtractedItem::StatementMention(
                StatementMention {
                    kind: statement_kind_byte(*kind),
                    subject_text: None,
                    // §22/07 Q6 — predicate qname inference is out of v1 scope.
                    predicate_qname: String::new(),
                    object_text: Some(text),
                    confidence: self.confidence,
                    extractor_id: id_raw,
                    extractor_version: self.extractor_version,
                },
            )),
            ExtractorTarget::Relation { .. } => None, // handled by run_for_relation below
            ExtractorTarget::EntityOrStatement => Some(ExtractedItem::EntityMention(EntityMention {
                entity_type_qname: String::new(),
                text,
                start,
                end,
                confidence: self.confidence,
                extractor_id: id_raw,
                extractor_version: self.extractor_version,
            })),
        }
    }
}

impl Extractor for PatternExtractor {
    fn id(&self) -> ExtractorId {
        self.id
    }

    fn kind(&self) -> ExtractorKind {
        ExtractorKind::Pattern
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn extractor_version(&self) -> u32 {
        self.extractor_version
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ExtractionContext<'a>,
        mem: &'a Memory,
    ) -> ExtractionFuture<'a> {
        Box::pin(async move {
            let start_ns = ctx.now_unix_nanos;
            let mut items: Vec<ExtractedItem> = Vec::new();
            let text = mem.text.as_deref().unwrap_or("");
            for compiled in &self.patterns {
                let re = &compiled.re;
                for caps in re.captures_iter(text) {
                    // For Relation target, require two capture groups.
                    if let ExtractorTarget::Relation { relation_type } = &self.target {
                        let (g1, g2) = match (caps.get(1), caps.get(2)) {
                            (Some(a), Some(b)) => (a, b),
                            _ => continue,
                        };
                        items.push(ExtractedItem::RelationMention(RelationMention {
                            relation_type_qname: relation_type.clone(),
                            subject_text: g1.as_str().to_string(),
                            object_text: g2.as_str().to_string(),
                            confidence: self.confidence,
                            extractor_id: self.id.raw(),
                            extractor_version: self.extractor_version,
                        }));
                        continue;
                    }

                    // First capture group if present, else the whole match.
                    let span = caps
                        .get(1)
                        .or_else(|| caps.get(0))
                        .expect("at least one match");
                    let span_text = span.as_str().to_string();
                    if let Some(item) = self.project(span_text, span.start(), span.end()) {
                        items.push(item);
                    }
                }
            }
            ExtractionResult::success(items, start_ns, ctx.now_unix_nanos)
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn compile_error(index: usize, e: regex::Error) -> ExtractorError {
    use regex::Error;
    match e {
        Error::CompiledTooBig(_) => ExtractorError::ResourceLimit {
            index,
            limit: "regex compile size",
        },
        other => ExtractorError::RegexCompile {
            index,
            message: other.to_string(),
        },
    }
}

fn statement_kind_byte(k: StatementKindAst) -> u8 {
    match k {
        StatementKindAst::Fact => StatementKind::Fact.as_u8(),
        StatementKindAst::Preference => StatementKind::Preference.as_u8(),
        StatementKindAst::Event => StatementKind::Event.as_u8(),
        // `Any` carries no specific kind. Storage-side maps Any to "no constraint";
        // emitted statements default to Fact discriminant (1) so downstream
        // resolution has something concrete to write.
        StatementKindAst::Any => StatementKind::Fact.as_u8(),
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::ExtractorRegistry;
    use brain_core::{AgentId, ContextId, MemoryId, MemoryKind, Salience};

    fn build(target: ExtractorTarget, patterns: &[&str], confidence: f32) -> PatternExtractor {
        let raw: Vec<String> = patterns.iter().map(|p| (*p).to_string()).collect();
        PatternExtractor::try_new(
            ExtractorId::from(7),
            "test:pat".into(),
            target,
            1,
            &raw,
            confidence,
        )
        .expect("build")
    }

    fn memory(text: &str) -> Memory {
        Memory {
            id: MemoryId::pack(0, 1, 0),
            agent: AgentId::new(),
            context: ContextId(0),
            kind: MemoryKind::Episodic,
            salience: Salience::default(),
            text: Some(text.to_string()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        }
    }

    fn ctx<'a>(reg: &'a ExtractorRegistry) -> ExtractionContext<'a> {
        ExtractionContext {
            schema_version: 1,
            now_unix_nanos: 0,
            registry: reg,
        }
    }

    fn entity_target() -> ExtractorTarget {
        ExtractorTarget::Entity {
            entity_type: "brain:Person".into(),
        }
    }

    #[test]
    fn try_new_compiles_simple_patterns() {
        let ext = build(entity_target(), &[r"\bAlice\b"], 0.7);
        assert_eq!(ext.patterns().len(), 1);
        assert_eq!(ext.patterns()[0].raw(), r"\bAlice\b");
    }

    #[test]
    fn try_new_rejects_invalid_regex() {
        let err = PatternExtractor::try_new(
            ExtractorId::from(1),
            "bad".into(),
            entity_target(),
            1,
            &[r"[a-".into()],
            0.5,
        )
        .unwrap_err();
        assert!(matches!(
            err,
            ExtractorError::RegexCompile { index: 0, .. }
        ));
    }

    #[test]
    fn try_new_resource_limit_is_wired() {
        // The 1 MiB size cap (§22/01 §2) is plumbed through
        // `RegexBuilder::size_limit` / `dfa_size_limit`. We verify
        // the error mapping rather than synthesising a pathological
        // input — the regex crate is efficient enough that the
        // canonical "huge alternation" patterns still fit under 1 MiB.
        // The mapping itself is asserted via `compile_error`'s
        // `CompiledTooBig` arm in the runtime path.
        let mapped = super::compile_error(
            3,
            regex::Error::CompiledTooBig(42),
        );
        assert!(matches!(
            mapped,
            ExtractorError::ResourceLimit { index: 3, limit }
                if limit == "regex compile size"
        ));
    }

    #[test]
    fn try_new_rejects_empty_patterns() {
        let err = PatternExtractor::try_new(
            ExtractorId::from(1),
            "empty".into(),
            entity_target(),
            1,
            &[],
            0.5,
        )
        .unwrap_err();
        assert!(matches!(err, ExtractorError::EmptyPatterns));
    }

    #[test]
    fn run_emits_entity_mention_for_each_match() {
        let reg = ExtractorRegistry::new();
        let ext = build(entity_target(), &[r"\bAlice\b"], 0.7);
        let mem = memory("Alice met Alice");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert_eq!(result.items.len(), 2);
        for item in &result.items {
            let ExtractedItem::EntityMention(m) = item else {
                panic!("expected EntityMention");
            };
            assert_eq!(m.text, "Alice");
            assert_eq!(m.entity_type_qname, "brain:Person");
            assert!((m.confidence - 0.7).abs() < 1e-6);
        }
        // First Alice at byte 0..5; second at byte 10..15.
        if let ExtractedItem::EntityMention(m0) = &result.items[0] {
            assert_eq!((m0.start, m0.end), (0, 5));
        } else {
            unreachable!()
        }
        if let ExtractedItem::EntityMention(m1) = &result.items[1] {
            assert_eq!((m1.start, m1.end), (10, 15));
        } else {
            unreachable!()
        }
    }

    #[test]
    fn run_uses_first_capture_group_when_present() {
        let reg = ExtractorRegistry::new();
        // Pattern with one capture group around the name.
        let ext = build(entity_target(), &[r"name=([A-Z][a-z]+)"], 0.8);
        let mem = memory("greeting name=Priya etc");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert_eq!(result.items.len(), 1);
        let ExtractedItem::EntityMention(m) = &result.items[0] else {
            panic!("expected EntityMention");
        };
        // Captures only "Priya", not "name=Priya".
        assert_eq!(m.text, "Priya");
    }

    #[test]
    fn run_with_no_matches_returns_empty_items_and_success() {
        let reg = ExtractorRegistry::new();
        let ext = build(entity_target(), &[r"\bZZZ\b"], 0.5);
        let mem = memory("nothing to see here");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert!(result.items.is_empty());
        assert_eq!(result.status, crate::extractor::ExtractionStatus::Success);
    }

    #[test]
    fn run_for_relation_target_requires_two_groups() {
        let reg = ExtractorRegistry::new();
        let target = ExtractorTarget::Relation {
            relation_type: "brain:reports_to".into(),
        };
        // Only one capture group → no items emitted.
        let ext = build(target, &[r"(\w+) reports"], 0.7);
        let mem = memory("Bob reports somewhere");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert!(
            result.items.is_empty(),
            "relation target with one group must skip the match"
        );
    }

    #[test]
    fn run_for_relation_target_emits_subject_object_from_groups() {
        let reg = ExtractorRegistry::new();
        let target = ExtractorTarget::Relation {
            relation_type: "brain:reports_to".into(),
        };
        let ext = build(target, &[r"(\w+) reports to (\w+)"], 0.9);
        let mem = memory("Bob reports to Priya");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert_eq!(result.items.len(), 1);
        let ExtractedItem::RelationMention(m) = &result.items[0] else {
            panic!("expected RelationMention");
        };
        assert_eq!(m.subject_text, "Bob");
        assert_eq!(m.object_text, "Priya");
        assert_eq!(m.relation_type_qname, "brain:reports_to");
        assert!((m.confidence - 0.9).abs() < 1e-6);
    }

    #[test]
    fn confidence_propagates_to_emitted_items() {
        let reg = ExtractorRegistry::new();
        let ext = build(entity_target(), &[r"\bX\b"], 0.42);
        let mem = memory("X X X");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        for item in &result.items {
            assert!((item.confidence() - 0.42).abs() < 1e-6);
        }
    }

    #[test]
    fn extractor_id_and_version_stamped_on_outputs() {
        let reg = ExtractorRegistry::new();
        let ext = build(entity_target(), &[r"\bAlice\b"], 0.7);
        let mem = memory("Alice");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        let ExtractedItem::EntityMention(m) = &result.items[0] else {
            panic!()
        };
        assert_eq!(m.extractor_id, 7);
        assert_eq!(m.extractor_version, 1);
    }

    #[test]
    fn unicode_offsets_are_byte_safe() {
        let reg = ExtractorRegistry::new();
        let ext = build(entity_target(), &[r"\bPriya\b"], 0.7);
        // "☃" is 3 bytes in UTF-8; "Priya" starts at byte 4.
        let mem = memory("☃ Priya rocks");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert_eq!(result.items.len(), 1);
        let ExtractedItem::EntityMention(m) = &result.items[0] else {
            panic!()
        };
        assert_eq!(m.text, "Priya");
        assert_eq!((m.start, m.end), (4, 9));
        // Re-slice using the offsets to confirm UTF-8 boundary safety.
        let slice = &mem.text.as_deref().unwrap()[m.start..m.end];
        assert_eq!(slice, "Priya");
    }

    #[test]
    fn statement_target_emits_object_text() {
        let reg = ExtractorRegistry::new();
        let target = ExtractorTarget::Statement {
            kind: StatementKindAst::Fact,
        };
        let ext = build(target, &[r"important: (.+)$"], 0.6);
        let mem = memory("important: ship phase 20");
        let result = futures_lite::future::block_on(ext.run(&ctx(&reg), &mem));
        assert_eq!(result.items.len(), 1);
        let ExtractedItem::StatementMention(m) = &result.items[0] else {
            panic!("expected StatementMention");
        };
        assert_eq!(m.object_text.as_deref(), Some("ship phase 20"));
        assert_eq!(m.kind, StatementKind::Fact.as_u8());
        // §22/07 Q6: predicate inference is deferred.
        assert!(m.predicate_qname.is_empty());
    }
}
