//! Convert persisted [`brain_metadata::ExtractorDefinition`] rows
//! into runtime `Arc<dyn Extractor>` instances. Spec §22 +
//! §21/05 §1.
//!
//! Called once at server / shard startup to populate the
//! in-memory [`crate::ExtractorRegistry`] from
//! `EXTRACTORS_TABLE` rows. Per-row decode failures are returned
//! alongside the populated registry — callers log them and
//! proceed; the substrate stays usable even with one or more
//! broken extractor definitions.

use std::sync::Arc;

use brain_core::ExtractorKind;
use brain_metadata::tables::knowledge::extractor::ExtractorDefinition;
use brain_protocol::schema::{
    ExtractorDef, ExtractorField, ExtractorKindAst, ExtractorTarget,
};

use crate::classifier::{ClassifierExtractor, ClassifierModel};
use crate::extractor::ExtractorError;
use crate::pattern::PatternExtractor;
use crate::registry::ExtractorRegistry;

/// Materialise a pattern extractor from a persisted row. Decodes
/// the JSON-encoded AST blob and constructs the runtime instance.
pub fn materialize_pattern_extractor(
    def: &ExtractorDefinition,
) -> Result<PatternExtractor, ExtractorError> {
    if def.kind() != Some(ExtractorKind::Pattern) {
        return Err(ExtractorError::OutputDecodeFailed {
            reason: format!(
                "definition kind byte {} is not Pattern",
                def.kind
            ),
        });
    }
    let ast = decode_definition_blob(&def.definition_blob)?;
    let patterns = extract_patterns(&ast);
    let confidence = extract_confidence(&ast).unwrap_or(0.7);
    PatternExtractor::try_new(
        def.id(),
        def.qname(),
        ast.target,
        def.schema_version,
        &patterns,
        confidence,
    )
}

/// Materialise a classifier extractor from a persisted row. If
/// `model` is `Some`, the extractor runs against it; if `None`,
/// returns a degraded extractor whose every dispatch writes
/// `Failure(reason: ...)` audit rows.
pub fn materialize_classifier_extractor(
    def: &ExtractorDefinition,
    model: Option<Arc<dyn ClassifierModel>>,
) -> Result<ClassifierExtractor, ExtractorError> {
    if def.kind() != Some(ExtractorKind::Classifier) {
        return Err(ExtractorError::OutputDecodeFailed {
            reason: format!(
                "definition kind byte {} is not Classifier",
                def.kind
            ),
        });
    }
    let ast = decode_definition_blob(&def.definition_blob)?;
    let threshold = extract_confidence_threshold(&ast).unwrap_or(0.6);
    let ext = match model {
        Some(m) => ClassifierExtractor::new(
            def.id(),
            def.qname(),
            ast.target,
            def.schema_version,
            threshold,
            m,
        ),
        None => ClassifierExtractor::degraded(
            def.id(),
            def.qname(),
            ast.target,
            def.schema_version,
            threshold,
            "classifier model not loaded — set BRAIN_NER_MODEL_PATH",
        ),
    };
    Ok(ext)
}

/// Top-level registry loader. Walks the persisted definitions,
/// materialises each via the kind-specific path, registers the
/// runtime instance, and collects per-row errors for diagnostic
/// logging. LLM-kind rows register as degraded classifier-shaped
/// placeholders pending phase 21.
///
/// The returned registry MAY be partial — the caller decides what
/// to do with errors. The recommended pattern is `tracing::warn`
/// each error then proceed.
#[must_use]
pub fn build_registry_from_definitions(
    defs: &[ExtractorDefinition],
    classifier_model: Option<Arc<dyn ClassifierModel>>,
) -> (
    ExtractorRegistry,
    Vec<(brain_core::ExtractorId, ExtractorError)>,
) {
    let mut registry = ExtractorRegistry::new();
    let mut errors: Vec<(brain_core::ExtractorId, ExtractorError)> = Vec::new();

    for def in defs {
        let id = def.id();
        match def.kind() {
            Some(ExtractorKind::Pattern) => match materialize_pattern_extractor(def) {
                Ok(p) => registry.register(Arc::new(p)),
                Err(e) => errors.push((id, e)),
            },
            Some(ExtractorKind::Classifier) => {
                match materialize_classifier_extractor(def, classifier_model.clone()) {
                    Ok(c) => registry.register(Arc::new(c)),
                    Err(e) => errors.push((id, e)),
                }
            }
            Some(ExtractorKind::Llm) => {
                // LLM tier is phase 21. Register as a degraded
                // classifier-shaped placeholder so the registry
                // surfaces the row but dispatches write
                // `Failure(reason: "llm tier pending phase 21")`.
                match decode_definition_blob(&def.definition_blob) {
                    Ok(ast) => {
                        let placeholder = ClassifierExtractor::degraded(
                            id,
                            def.qname(),
                            ast.target,
                            def.schema_version,
                            0.0,
                            "llm tier pending phase 21",
                        );
                        registry.register(Arc::new(placeholder));
                    }
                    Err(e) => errors.push((id, e)),
                }
            }
            None => errors.push((
                id,
                ExtractorError::OutputDecodeFailed {
                    reason: format!("unknown extractor kind byte {}", def.kind),
                },
            )),
        }

        // Respect the persisted `enabled` flag.
        if !def.is_enabled() {
            registry.set_enabled(id, false);
        }
    }

    (registry, errors)
}

// ---------------------------------------------------------------------------
// Helpers.
// ---------------------------------------------------------------------------

fn decode_definition_blob(blob: &[u8]) -> Result<ExtractorDef, ExtractorError> {
    serde_json::from_slice::<ExtractorDef>(blob).map_err(|e| ExtractorError::OutputDecodeFailed {
        reason: format!("definition_blob JSON decode failed: {e}"),
    })
}

fn extract_patterns(ast: &ExtractorDef) -> Vec<String> {
    for f in &ast.fields {
        if let ExtractorField::Patterns(p) = f {
            return p.clone();
        }
    }
    Vec::new()
}

fn extract_confidence(ast: &ExtractorDef) -> Option<f32> {
    for f in &ast.fields {
        if let ExtractorField::Confidence(c) = f {
            return Some(*c);
        }
    }
    None
}

fn extract_confidence_threshold(ast: &ExtractorDef) -> Option<f32> {
    for f in &ast.fields {
        if let ExtractorField::ConfidenceThreshold(c) = f {
            return Some(*c);
        }
    }
    None
}

// Quiet unused-import warnings while AST surface remains stable.
#[allow(dead_code)]
fn _ensure_imports(k: ExtractorKindAst, t: ExtractorTarget) -> (ExtractorKindAst, ExtractorTarget) {
    (k, t)
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::Extractor;
    use brain_protocol::schema::{
        ExtractorDef as AstExtractorDef, ExtractorField, ExtractorKindAst, ExtractorTarget,
    };

    fn pattern_def_blob() -> Vec<u8> {
        let ast = AstExtractorDef {
            name: "person_mentions".into(),
            kind: ExtractorKindAst::Pattern,
            target: ExtractorTarget::Entity {
                entity_type: "brain:Person".into(),
            },
            fields: vec![
                ExtractorField::Patterns(vec![r"\b([A-Z][a-z]+)\b".into()]),
                ExtractorField::Confidence(0.75),
            ],
        };
        serde_json::to_vec(&ast).unwrap()
    }

    fn classifier_def_blob() -> Vec<u8> {
        let ast = AstExtractorDef {
            name: "basic_ner".into(),
            kind: ExtractorKindAst::Classifier,
            target: ExtractorTarget::Entity {
                entity_type: "brain:Person".into(),
            },
            fields: vec![
                ExtractorField::Model("brain-basic-ner-v1".into()),
                ExtractorField::ConfidenceThreshold(0.6),
            ],
        };
        serde_json::to_vec(&ast).unwrap()
    }

    fn llm_def_blob() -> Vec<u8> {
        let ast = AstExtractorDef {
            name: "preferences".into(),
            kind: ExtractorKindAst::Llm,
            target: ExtractorTarget::Statement {
                kind: brain_protocol::schema::StatementKindAst::Preference,
            },
            fields: vec![
                ExtractorField::Model("claude-haiku".into()),
                ExtractorField::Prompt("extract".into()),
            ],
        };
        serde_json::to_vec(&ast).unwrap()
    }

    fn row(id: u32, kind: ExtractorKind, blob: Vec<u8>) -> ExtractorDefinition {
        ExtractorDefinition::new(
            brain_core::ExtractorId::from(id),
            "brain".into(),
            "test".into(),
            kind,
            true,
            1,
            blob,
            0,
        )
    }

    #[test]
    fn materialize_pattern_decodes_definition_blob() {
        let r = row(1, ExtractorKind::Pattern, pattern_def_blob());
        let p = materialize_pattern_extractor(&r).expect("materialize");
        assert_eq!(p.id().raw(), 1);
        assert_eq!(p.patterns().len(), 1);
        assert!((p.confidence() - 0.75).abs() < 1e-6);
    }

    #[test]
    fn materialize_pattern_fails_on_invalid_blob() {
        let r = row(1, ExtractorKind::Pattern, b"not-json".to_vec());
        let err = materialize_pattern_extractor(&r).unwrap_err();
        assert!(matches!(
            err,
            ExtractorError::OutputDecodeFailed { ref reason }
                if reason.contains("JSON decode")
        ));
    }

    #[test]
    fn materialize_pattern_fails_on_empty_patterns() {
        let ast = AstExtractorDef {
            name: "noop".into(),
            kind: ExtractorKindAst::Pattern,
            target: ExtractorTarget::Entity {
                entity_type: "brain:Person".into(),
            },
            fields: vec![ExtractorField::Confidence(0.7)],
        };
        let blob = serde_json::to_vec(&ast).unwrap();
        let r = row(1, ExtractorKind::Pattern, blob);
        let err = materialize_pattern_extractor(&r).unwrap_err();
        assert!(matches!(err, ExtractorError::EmptyPatterns));
    }

    #[test]
    fn materialize_pattern_rejects_classifier_kind() {
        let r = row(1, ExtractorKind::Classifier, pattern_def_blob());
        let err = materialize_pattern_extractor(&r).unwrap_err();
        assert!(matches!(
            err,
            ExtractorError::OutputDecodeFailed { ref reason }
                if reason.contains("not Pattern")
        ));
    }

    #[test]
    fn materialize_classifier_without_model_is_degraded() {
        let r = row(1, ExtractorKind::Classifier, classifier_def_blob());
        let c = materialize_classifier_extractor(&r, None).expect("materialize");
        assert!(!c.is_loaded());
    }

    #[test]
    fn materialize_classifier_with_model_is_loaded() {
        struct DummyModel;
        impl ClassifierModel for DummyModel {
            fn predict(
                &self,
                _text: &str,
            ) -> Result<Vec<crate::classifier::TokenClassification>, ExtractorError> {
                Ok(vec![])
            }
            fn version(&self) -> &str {
                "dummy"
            }
        }
        let r = row(1, ExtractorKind::Classifier, classifier_def_blob());
        let c = materialize_classifier_extractor(&r, Some(Arc::new(DummyModel))).unwrap();
        assert!(c.is_loaded());
    }

    #[test]
    fn build_registry_collects_errors_per_row() {
        let defs = vec![
            row(1, ExtractorKind::Pattern, pattern_def_blob()),
            row(2, ExtractorKind::Pattern, b"bad".to_vec()),
            row(3, ExtractorKind::Classifier, classifier_def_blob()),
        ];
        let (reg, errs) = build_registry_from_definitions(&defs, None);
        assert_eq!(reg.len(), 2, "valid rows registered");
        assert_eq!(errs.len(), 1, "bad row produces error");
        assert_eq!(errs[0].0.raw(), 2);
    }

    #[test]
    fn build_registry_handles_llm_kind_as_degraded() {
        let defs = vec![row(1, ExtractorKind::Llm, llm_def_blob())];
        let (reg, errs) = build_registry_from_definitions(&defs, None);
        assert_eq!(reg.len(), 1);
        assert!(errs.is_empty());
        // It registers but iter_enabled returns it (enabled by default
        // from the row's `is_enabled` flag).
        assert_eq!(reg.iter_enabled().count(), 1);
    }

    #[test]
    fn build_registry_respects_disabled_flag() {
        let mut def = row(1, ExtractorKind::Pattern, pattern_def_blob());
        def.enabled = 0;
        let defs = vec![def];
        let (reg, _) = build_registry_from_definitions(&defs, None);
        assert_eq!(reg.iter_enabled().count(), 0);
        assert_eq!(reg.iter_all().count(), 1);
    }
}
