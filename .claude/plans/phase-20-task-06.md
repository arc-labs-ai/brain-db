# 20.6 — ENCODE handler integration

Wires the extractor pipeline into the ENCODE non-txn path. Pattern
extractors run synchronously post-commit; classifier extractors
dispatch in the same call but remain in degraded state until 20.7
flips on BertRuntime. Every dispatch writes one audit row regardless
of outcome.

## Scope cut: defer BertRuntime candle wiring to 20.7

Phase 20.3 staged the BertTokenClassifier load path with the
candle forward pass returning `InferenceFailed { reason: "runtime
not wired" }`. Phase 20.6 ships the pipeline that **calls** the
classifier — but the runtime stays unwired. Concretely:

- Pattern extractors run end-to-end → audit row with items.
- Classifier extractors dispatch → predict returns the staged
  error → audit row with `Failure(reason: "runtime not wired")`.
  ENCODE doesn't fail.

20.7 lands the real candle forward pass + system-schema bootstrap
of `brain.basic_ner` together so the end-to-end "model registered
+ inference runs" path is testable in one commit.

This split keeps 20.6 focused on the **integration surface**
(OpsContext, pipeline, ENCODE hook) and 20.7 focused on the
**bundled built-ins lighting up** (system schema + candle).

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-extractors/src/materialize.rs` | New: decode `ExtractorDefinition.definition_blob` → `Arc<dyn Extractor>`. |
| `crates/brain-extractors/src/lib.rs` | Add `pub mod materialize;` + re-exports. |
| `crates/brain-ops/src/context.rs` | Add `extractor_registry` + `classifier_config` fields; new builder methods. |
| `crates/brain-ops/src/ops/extractor_pipeline.rs` | New: `run_extractor_pipeline(ctx, &Memory)`. |
| `crates/brain-ops/src/ops/mod.rs` | Add `pub mod extractor_pipeline;`. |
| `crates/brain-ops/src/lib.rs` | Re-export `extractor_pipeline`. |
| `crates/brain-ops/src/ops/encode.rs` | Hook the pipeline after `execute_encode` returns. |
| `crates/brain-ops/Cargo.toml` | Add `brain-extractors` dep. |

## materialize.rs (brain-extractors)

```rust
pub fn materialize_pattern_extractor(
    def: &ExtractorDefinition,
) -> Result<PatternExtractor, ExtractorError>;

/// Build a classifier extractor from a row. If `model` is `Some`,
/// the extractor runs against it; if `None`, returns a degraded
/// extractor that writes `Failure` on every dispatch.
pub fn materialize_classifier_extractor(
    def: &ExtractorDefinition,
    model: Option<Arc<dyn ClassifierModel>>,
) -> Result<ClassifierExtractor, ExtractorError>;

/// Top-level loader. Walks all `ExtractorDefinition` rows, decodes
/// `definition_blob` to `ExtractorDef`, materialises pattern /
/// classifier impls, registers them. Per-row errors are collected
/// and returned alongside the populated registry — callers log
/// them and proceed (degraded extractors continue to register and
/// surface failures via audit on dispatch).
pub fn build_registry_from_definitions(
    defs: &[ExtractorDefinition],
    classifier_model: Option<Arc<dyn ClassifierModel>>,
) -> (ExtractorRegistry, Vec<(ExtractorId, ExtractorError)>);
```

LLM extractors decode-and-skip in 20.6 (no impl yet); they
register as degraded `ClassifierExtractor` placeholders with
reason "llm tier pending phase 21".

## OpsContext changes

```rust
pub struct OpsContext {
    // existing fields
    pub extractor_registry: Arc<parking_lot::RwLock<ExtractorRegistry>>,
    pub classifier_config: Arc<ClassifierConfig>,
}

impl OpsContext {
    pub fn with_classifier_config(mut self, cfg: ClassifierConfig) -> Self { ... }
    pub fn with_extractor_registry(mut self, reg: ExtractorRegistry) -> Self { ... }
}
```

`new(executor)` defaults to:
- `extractor_registry: empty` — populated at startup by phase 20.7
  (system schema bootstrap registers built-ins).
- `classifier_config: ClassifierConfig::unloaded()` — operators
  set via `with_classifier_config`.

## extractor_pipeline.rs (brain-ops)

```rust
/// Run every enabled extractor over `memory` synchronously. Writes
/// one audit row per dispatch. Errors are logged + audited — never
/// returned to the caller. Called by `handle_encode` after the
/// WAL commit completes.
pub async fn run_extractor_pipeline(ctx: &OpsContext, memory: &Memory);
```

Behaviour:
1. Snapshot the registry under a read lock; collect `Arc<dyn Extractor>` references.
2. For each extractor (no specific order in 20.6 — `depends_on`
   topology is §22/07 Q11 follow-up):
   a. Build `ExtractionContext { schema_version, now, registry }`.
   b. Call `extractor.run(&ctx, &memory)`.
   c. Build `ExtractionAudit` from the result. `outputs: vec![]`
      in 20.6 — persistence of `EntityMention` rows lands in phase
      22+ (§22/04 resolver tier). The audit row's
      `status_reason` field carries the item count
      ("3 items produced (resolver pending)") when status=Success
      and items are non-empty.
   d. Open a wtxn → `audit_write` → commit.
3. On audit-write failure: trace::warn but don't propagate.

## ENCODE call-site

End of `handle_encode` non-txn path, after `execute_encode` returns:

```rust
let memory = Memory {
    id: result.memory_id,
    agent: agent_from_req,
    context: context_from_req,
    kind: req.kind.into(),
    salience: Salience::new(salience),
    text: Some(req.text.clone()),
    created_at_unix_ms: ...,
    last_accessed_at_unix_ms: ...,
};
let _ = run_extractor_pipeline(ctx, &memory).await;
```

Txn path skipped: extractors run on commit (phase 22+).

## Tests

`materialize.rs`:

1. `materialize_pattern_decodes_definition_blob` — round-trip
   `ExtractorDef` → bytes → `PatternExtractor`.
2. `materialize_pattern_fails_on_invalid_blob` — bad JSON.
3. `materialize_pattern_fails_on_empty_patterns`.
4. `materialize_classifier_without_model_is_degraded`.
5. `materialize_classifier_with_model_is_loaded`.
6. `build_registry_collects_errors_per_row` — mixed valid +
   invalid defs; registry contains the valid ones, error list has
   the invalid ones.
7. `build_registry_handles_llm_kind_as_degraded`.

`extractor_pipeline.rs`:

8. Pipeline integration via a mock extractor that records dispatch
   + asserts audit row written.

ENCODE end-to-end tests deferred to 20.9.

## Out of scope

- Real candle forward pass — 20.7.
- Entity-mention persistence via resolver tier — phase 22+.
- Classifier near-foreground queue — §27/07 Q5.
- LLM extractor dispatch — phase 21.
- `depends_on` topological ordering — §22/07 Q11.
- Backpressure on queue overflow — §27/01 §6.

## Single commit

`feat(extractors,ops): 20.6 — extractor pipeline + ENCODE integration`

## Verification

```
just docker cargo test -p brain-extractors --lib materialize
just docker cargo test -p brain-ops --lib extractor_pipeline
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
