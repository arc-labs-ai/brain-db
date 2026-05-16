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
use std::time::Duration;

use brain_core::ExtractorKind;
use brain_llm::ModelRouter;
use brain_metadata::tables::knowledge::extractor::ExtractorDefinition;
use brain_metadata::LlmCacheDb;
use brain_protocol::schema::ast::{CacheConfig, CostExpr, CostUnit, DurationAst, DurationUnit};
use brain_protocol::schema::{
    ExtractorDef, ExtractorField, ExtractorKindAst, ExtractorTarget,
};
use parking_lot::Mutex;
use serde_json::Value;

use crate::classifier::{ClassifierExtractor, ClassifierModel};
use crate::extractor::ExtractorError;
use crate::llm::{CostBudget, LlmExtractor};
use crate::pattern::PatternExtractor;
use crate::registry::ExtractorRegistry;

const DEFAULT_LLM_CACHE_TTL_SECS: u64 = 7 * 24 * 60 * 60;
const DEFAULT_LLM_CONFIDENCE_THRESHOLD: f32 = 0.7;

/// Bundle of optional dependencies the materializer needs to
/// build the three extractor kinds. Phase 21.5 wires `model_router`
/// + `llm_cache` at shard startup; before that, callers pass
/// [`MaterializeDeps::default()`] (every field `None`) and LLM
/// rows register as degraded.
#[derive(Clone, Default)]
pub struct MaterializeDeps {
    pub classifier_model: Option<Arc<dyn ClassifierModel>>,
    pub model_router: Option<Arc<ModelRouter>>,
    pub llm_cache: Option<Arc<Mutex<LlmCacheDb>>>,
}

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

/// Materialise an LLM extractor from a persisted row.
///
/// `Err` is reserved for true decode failures (bad JSON blob,
/// kind mismatch). Every operator-visible misconfiguration —
/// missing required field, unknown model, schema compile failure,
/// unsupported cost-unit — returns a wired
/// [`LlmExtractor::degraded`] whose dispatches emit a `Failure`
/// audit row with the captured reason. The registry stays
/// populated; ENCODE stays non-blocking.
pub fn materialize_llm_extractor(
    def: &ExtractorDefinition,
    deps: &MaterializeDeps,
) -> Result<LlmExtractor, ExtractorError> {
    if def.kind() != Some(ExtractorKind::Llm) {
        return Err(ExtractorError::OutputDecodeFailed {
            reason: format!("definition kind byte {} is not Llm", def.kind),
        });
    }
    let ast = decode_definition_blob(&def.definition_blob)?;
    let id = def.id();
    let name = def.qname();
    let target = ast.target.clone();
    let version = def.schema_version;
    let threshold = extract_confidence_threshold(&ast).unwrap_or(DEFAULT_LLM_CONFIDENCE_THRESHOLD);

    // Required fields.
    let Some(model) = extract_model(&ast) else {
        return Ok(LlmExtractor::degraded(
            id,
            name,
            target,
            version,
            threshold,
            "llm extractor missing required 'model' field",
        ));
    };
    let Some(prompt) = extract_prompt(&ast) else {
        return Ok(LlmExtractor::degraded(
            id,
            name,
            target,
            version,
            threshold,
            "llm extractor missing required 'prompt' field",
        ));
    };

    // Cost budget translation.
    let cost_budget = match extract_cost_budget(&ast) {
        CostBudgetExtract::Unset => None,
        CostBudgetExtract::PerRequest(micro) => Some(CostBudget {
            per_call_micro_usd: micro,
        }),
        CostBudgetExtract::Unsupported(reason) => {
            return Ok(LlmExtractor::degraded(
                id, name, target, version, threshold, reason,
            ));
        }
    };

    // Router resolution.
    let Some(router) = deps.model_router.as_ref() else {
        return Ok(LlmExtractor::degraded(
            id,
            name,
            target,
            version,
            threshold,
            "no llm clients configured (set ANTHROPIC_API_KEY or OPENAI_API_KEY)",
        ));
    };
    let Some(client) = router.resolve(model) else {
        let provider = brain_llm::Provider::classify(model);
        return Ok(LlmExtractor::degraded(
            id,
            name,
            target,
            version,
            threshold,
            format!(
                "no client configured for model {model} (provider {})",
                provider.name()
            ),
        ));
    };

    // Schema compile.
    let response_schema = extract_response_schema(&ast);
    let schema_compiled = match LlmExtractor::compile_schema(response_schema.as_ref()) {
        Ok(s) => s,
        Err(e) => {
            return Ok(LlmExtractor::degraded(
                id,
                name,
                target,
                version,
                threshold,
                format!("response_schema invalid: {e}"),
            ));
        }
    };

    // Cache wiring: only attach the operator-provided cache when
    // the schema says caching is enabled (default).
    let cache = match extract_cache_config(&ast) {
        CacheConfig::Disabled => None,
        CacheConfig::Enabled => deps.llm_cache.clone(),
    };
    let cache_ttl = extract_cache_ttl(&ast)
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_LLM_CACHE_TTL_SECS));

    let examples = extract_examples(&ast);
    let extractor = LlmExtractor::build(
        id,
        name,
        target,
        version,
        client,
        cache,
        prompt.to_string(),
        examples,
        response_schema,
        schema_compiled,
        threshold,
        cost_budget,
        cache_ttl,
    );
    Ok(extractor)
}

/// Top-level registry loader. Walks the persisted definitions,
/// materialises each via the kind-specific path, registers the
/// runtime instance, and collects per-row errors for diagnostic
/// logging. LLM-kind rows now go through
/// [`materialize_llm_extractor`]; rows whose operator
/// configuration is incomplete (missing keys, unknown models,
/// bad schemas) register as `LlmExtractor::degraded` with an
/// actionable reason instead of being dropped.
///
/// The returned registry MAY be partial — the caller decides what
/// to do with errors. The recommended pattern is `tracing::warn`
/// each error then proceed.
#[must_use]
pub fn build_registry_from_definitions(
    defs: &[ExtractorDefinition],
    deps: &MaterializeDeps,
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
                match materialize_classifier_extractor(def, deps.classifier_model.clone()) {
                    Ok(c) => registry.register(Arc::new(c)),
                    Err(e) => errors.push((id, e)),
                }
            }
            Some(ExtractorKind::Llm) => match materialize_llm_extractor(def, deps) {
                Ok(l) => registry.register(Arc::new(l)),
                Err(e) => errors.push((id, e)),
            },
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

fn extract_model(ast: &ExtractorDef) -> Option<&str> {
    for f in &ast.fields {
        if let ExtractorField::Model(m) = f {
            return Some(m.as_str());
        }
    }
    None
}

fn extract_prompt(ast: &ExtractorDef) -> Option<&str> {
    for f in &ast.fields {
        if let ExtractorField::Prompt(p) = f {
            return Some(p.as_str());
        }
    }
    None
}

fn extract_examples(ast: &ExtractorDef) -> Option<Value> {
    for f in &ast.fields {
        if let ExtractorField::Examples(v) = f {
            return Some(v.clone());
        }
    }
    None
}

fn extract_response_schema(ast: &ExtractorDef) -> Option<Value> {
    for f in &ast.fields {
        if let ExtractorField::Schema(v) = f {
            return Some(v.clone());
        }
    }
    None
}

fn extract_cache_config(ast: &ExtractorDef) -> CacheConfig {
    for f in &ast.fields {
        if let ExtractorField::Cache(c) = f {
            return *c;
        }
    }
    CacheConfig::Enabled
}

fn extract_cache_ttl(ast: &ExtractorDef) -> Option<Duration> {
    for f in &ast.fields {
        if let ExtractorField::CacheTtl(d) = f {
            return Some(duration_ast_to_duration(*d));
        }
    }
    None
}

/// Outcome of cost-budget extraction. Phase 21 supports
/// `PerRequest` only (§22/09 §5); the other variants land as
/// degraded extractors with operator-actionable reasons.
enum CostBudgetExtract {
    Unset,
    PerRequest(u64),
    Unsupported(String),
}

fn extract_cost_budget(ast: &ExtractorDef) -> CostBudgetExtract {
    for f in &ast.fields {
        if let ExtractorField::CostBudget(c) = f {
            return cost_expr_to_budget(*c);
        }
    }
    CostBudgetExtract::Unset
}

fn cost_expr_to_budget(c: CostExpr) -> CostBudgetExtract {
    match c.unit {
        CostUnit::PerRequest => {
            let micro = (c.amount * 1_000_000.0).round();
            let micro = if micro.is_finite() && micro >= 0.0 {
                micro as u64
            } else {
                0
            };
            CostBudgetExtract::PerRequest(micro)
        }
        CostUnit::PerMemory => CostBudgetExtract::Unsupported(
            "cost_budget unit per_memory not supported in v1 (use per_request)".into(),
        ),
        CostUnit::PerDay => CostBudgetExtract::Unsupported(
            "cost_budget unit per_day not supported in v1 (use per_request)".into(),
        ),
    }
}

fn duration_ast_to_duration(d: DurationAst) -> Duration {
    let secs = match d.unit {
        DurationUnit::Seconds => d.amount,
        DurationUnit::Minutes => d.amount.saturating_mul(60),
        DurationUnit::Hours => d.amount.saturating_mul(3_600),
        DurationUnit::Days => d.amount.saturating_mul(86_400),
    };
    Duration::from_secs(secs)
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
        let (reg, errs) = build_registry_from_definitions(&defs, &MaterializeDeps::default());
        assert_eq!(reg.len(), 2, "valid rows registered");
        assert_eq!(errs.len(), 1, "bad row produces error");
        assert_eq!(errs[0].0.raw(), 2);
    }

    #[test]
    fn build_registry_handles_llm_kind_as_degraded() {
        let defs = vec![row(1, ExtractorKind::Llm, llm_def_blob())];
        let (reg, errs) = build_registry_from_definitions(&defs, &MaterializeDeps::default());
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
        let (reg, _) = build_registry_from_definitions(&defs, &MaterializeDeps::default());
        assert_eq!(reg.iter_enabled().count(), 0);
        assert_eq!(reg.iter_all().count(), 1);
    }

    // ----- 21.4 LLM materialization -----------------------------------------

    use brain_llm::client::LlmFuture;
    use brain_llm::{LlmClient, LlmError, LlmRequest, ModelRouter};
    use brain_protocol::schema::ast::{
        CacheConfig as AstCacheConfig, CostExpr, CostUnit, DurationAst, DurationUnit,
        StatementKindAst,
    };

    struct FakeClient {
        model: String,
    }

    impl LlmClient for FakeClient {
        fn complete<'a>(&'a self, _request: LlmRequest) -> LlmFuture<'a> {
            Box::pin(async {
                Err(LlmError::ProviderError {
                    status: 500,
                    message: "fake".into(),
                })
            })
        }

        fn model(&self) -> &str {
            &self.model
        }

        fn model_id_hash(&self) -> u64 {
            brain_llm::client::model_id_hash(&self.model)
        }
    }

    fn anthropic_router() -> Arc<ModelRouter> {
        let client: Arc<dyn LlmClient> = Arc::new(FakeClient {
            model: "claude-haiku-4-5".into(),
        });
        Arc::new(ModelRouter::new().with_anthropic(client))
    }

    fn llm_ast(name: &str, fields: Vec<ExtractorField>) -> Vec<u8> {
        let ast = AstExtractorDef {
            name: name.into(),
            kind: ExtractorKindAst::Llm,
            target: ExtractorTarget::Statement {
                kind: StatementKindAst::Preference,
            },
            fields,
        };
        serde_json::to_vec(&ast).unwrap()
    }

    #[test]
    fn materialize_llm_decodes_full_definition() {
        let schema = serde_json::json!({"type":"array","items":{"type":"string"}});
        let blob = llm_ast(
            "preferences",
            vec![
                ExtractorField::Model("claude-haiku-4-5".into()),
                ExtractorField::Prompt("extract preferences".into()),
                ExtractorField::Schema(schema),
                ExtractorField::ConfidenceThreshold(0.8),
                ExtractorField::CostBudget(CostExpr {
                    amount: 0.01,
                    unit: CostUnit::PerRequest,
                }),
                ExtractorField::Cache(AstCacheConfig::Disabled),
            ],
        );
        let r = row(7, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).expect("materialize");
        assert!(l.is_wired(), "fully configured row should be wired");
        assert_eq!(l.name(), "brain:test");
        assert_eq!(l.extractor_version(), 1);
    }

    #[test]
    fn materialize_llm_without_router_is_degraded() {
        let blob = llm_ast(
            "p",
            vec![
                ExtractorField::Model("claude-haiku-4-5".into()),
                ExtractorField::Prompt("x".into()),
            ],
        );
        let r = row(1, ExtractorKind::Llm, blob);
        let l = materialize_llm_extractor(&r, &MaterializeDeps::default()).unwrap();
        assert!(!l.is_wired());
        let reg = ExtractorRegistry::new();
        let mem = brain_core::Memory {
            id: brain_core::MemoryId::pack(0, 1, 0),
            agent: brain_core::AgentId::new(),
            context: brain_core::ContextId(0),
            kind: brain_core::MemoryKind::Episodic,
            salience: brain_core::Salience::default(),
            text: Some("hi".into()),
            created_at_unix_ms: 0,
            last_accessed_at_unix_ms: 0,
        };
        let ctx = crate::extractor::ExtractionContext {
            schema_version: 1,
            now_unix_nanos: 0,
            registry: &reg,
        };
        let r2 = futures_lite::future::block_on(l.run(&ctx, &mem));
        assert!(r2.status_reason.contains("no llm clients configured"));
    }

    #[test]
    fn materialize_llm_unknown_model_is_degraded() {
        let blob = llm_ast(
            "p",
            vec![
                ExtractorField::Model("llama-3".into()),
                ExtractorField::Prompt("x".into()),
            ],
        );
        let r = row(1, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).unwrap();
        assert!(!l.is_wired());
    }

    #[test]
    fn materialize_llm_unconfigured_provider_is_degraded() {
        // Router knows the prefix but the OpenAI client slot is empty.
        let blob = llm_ast(
            "p",
            vec![
                ExtractorField::Model("gpt-4o-mini".into()),
                ExtractorField::Prompt("x".into()),
            ],
        );
        let r = row(1, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).unwrap();
        assert!(!l.is_wired());
    }

    #[test]
    fn materialize_llm_missing_prompt_is_degraded() {
        let blob = llm_ast(
            "p",
            vec![ExtractorField::Model("claude-haiku-4-5".into())],
        );
        let r = row(1, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).unwrap();
        assert!(!l.is_wired());
    }

    #[test]
    fn materialize_llm_missing_model_is_degraded() {
        let blob = llm_ast("p", vec![ExtractorField::Prompt("x".into())]);
        let r = row(1, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).unwrap();
        assert!(!l.is_wired());
    }

    #[test]
    fn materialize_llm_bad_schema_is_degraded() {
        let bad_schema = serde_json::json!({"type": "not-a-type"});
        let blob = llm_ast(
            "p",
            vec![
                ExtractorField::Model("claude-haiku-4-5".into()),
                ExtractorField::Prompt("x".into()),
                ExtractorField::Schema(bad_schema),
            ],
        );
        let r = row(1, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).unwrap();
        assert!(!l.is_wired());
    }

    #[test]
    fn materialize_llm_cost_budget_per_memory_is_degraded() {
        let blob = llm_ast(
            "p",
            vec![
                ExtractorField::Model("claude-haiku-4-5".into()),
                ExtractorField::Prompt("x".into()),
                ExtractorField::CostBudget(CostExpr {
                    amount: 0.01,
                    unit: CostUnit::PerMemory,
                }),
            ],
        );
        let r = row(1, ExtractorKind::Llm, blob);
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let l = materialize_llm_extractor(&r, &deps).unwrap();
        assert!(!l.is_wired());
    }

    #[test]
    fn cost_expr_per_request_converts_to_micro_usd() {
        let b = cost_expr_to_budget(CostExpr {
            amount: 0.01,
            unit: CostUnit::PerRequest,
        });
        match b {
            CostBudgetExtract::PerRequest(m) => assert_eq!(m, 10_000),
            _ => panic!("expected PerRequest"),
        }
    }

    #[test]
    fn duration_ast_unit_conversions() {
        assert_eq!(
            duration_ast_to_duration(DurationAst {
                amount: 2,
                unit: DurationUnit::Days,
            }),
            Duration::from_secs(172_800),
        );
        assert_eq!(
            duration_ast_to_duration(DurationAst {
                amount: 5,
                unit: DurationUnit::Minutes,
            }),
            Duration::from_secs(300),
        );
    }

    #[test]
    fn build_registry_routes_llm_to_real_materializer() {
        let pat = row(1, ExtractorKind::Pattern, pattern_def_blob());
        let llm = row(
            2,
            ExtractorKind::Llm,
            llm_ast(
                "preferences",
                vec![
                    ExtractorField::Model("claude-haiku-4-5".into()),
                    ExtractorField::Prompt("extract".into()),
                ],
            ),
        );
        let deps = MaterializeDeps {
            classifier_model: None,
            model_router: Some(anthropic_router()),
            llm_cache: None,
        };
        let (reg, errs) = build_registry_from_definitions(&[pat, llm], &deps);
        assert!(errs.is_empty());
        assert_eq!(reg.iter_enabled().count(), 2);
        // The LLM row must be a real (wired) LlmExtractor — check via
        // kind() so we don't depend on `Any`-downcasting.
        let by_id = reg
            .lookup(brain_core::ExtractorId::from(2))
            .expect("llm registered");
        assert_eq!(by_id.kind(), brain_core::knowledge::ExtractorKind::Llm);
    }

    #[test]
    fn build_registry_llm_without_router_registers_degraded() {
        let llm = row(
            1,
            ExtractorKind::Llm,
            llm_ast(
                "preferences",
                vec![
                    ExtractorField::Model("claude-haiku-4-5".into()),
                    ExtractorField::Prompt("extract".into()),
                ],
            ),
        );
        let (reg, errs) =
            build_registry_from_definitions(&[llm], &MaterializeDeps::default());
        assert!(errs.is_empty());
        assert_eq!(reg.iter_enabled().count(), 1);
        // Still registered as kind=Llm — degradation is internal state.
        let by_id = reg
            .lookup(brain_core::ExtractorId::from(1))
            .expect("registered");
        assert_eq!(by_id.kind(), brain_core::knowledge::ExtractorKind::Llm);
    }
}
