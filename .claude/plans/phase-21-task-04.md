# 21.4 — Materialise + register LLM extractors

Extends `brain-extractors::materialize` so persisted
`ExtractorDefinition` rows with `kind == Llm` decode into real
`LlmExtractor` instances instead of degraded
`ClassifierExtractor` placeholders (the 20.6 stop-gap). Threads
the LLM-tier deps (model router + cache) through
`build_registry_from_definitions` so phase 21.5 only has to plug
the wires at the shard boundary.

## Scope cut: no server-side wiring here

21.5 owns:
- env-based `ANTHROPIC_API_KEY` / `OPENAI_API_KEY` reads,
- shard-startup `LlmCacheDb::open`,
- threading both into the shared `OpsContext`.

21.4 is purely the **materializer surface**: decoder + degraded-
fallbacks + tests. Callers still pass `None` everywhere; the
existing 20.6 / 20.7 behaviour is unchanged.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-extractors/src/materialize.rs` | New `materialize_llm_extractor`; new `MaterializeDeps` struct; `build_registry_from_definitions` migrated to take `MaterializeDeps`. LLM branch swaps the degraded-placeholder for the real call. |
| `crates/brain-extractors/src/lib.rs` | Re-export `materialize_llm_extractor` + `MaterializeDeps`. |
| `crates/brain-extractors/src/llm.rs` | Public helper `LlmExtractor::from_definition_fields` (extract the field-walking logic from the materializer so tests can exercise it cheaply). |
| `crates/brain-ops/src/ops/extractor_pipeline.rs` | Touch only if a call site needs the new `MaterializeDeps` signature — likely no change since `build_registry_from_definitions` is shard-boundary-only. |
| `crates/brain-server/src/shard/mod.rs` | Single-line: pass `MaterializeDeps { classifier_model, model_router: None, llm_cache: None }` instead of bare `classifier_model`. (Real wiring lands in 21.5.) |

No new workspace deps. `brain-extractors` already pulls `brain-llm`.

## materialize.rs additions

```rust
/// Bundle of optional dependencies the materializer needs to build
/// the three extractor kinds. Phase 21.5 wires `model_router` +
/// `llm_cache` at shard startup; before that, callers pass
/// `MaterializeDeps::default()` (every field `None`) and LLM rows
/// register as degraded.
#[derive(Clone, Default)]
pub struct MaterializeDeps {
    pub classifier_model: Option<Arc<dyn ClassifierModel>>,
    pub model_router: Option<Arc<ModelRouter>>,
    pub llm_cache: Option<Arc<parking_lot::Mutex<LlmCacheDb>>>,
}

/// Materialise an LLM extractor from a persisted row.
///
/// Degraded outcomes (each returns an `LlmExtractor::degraded(...)`
/// with the matching reason — never `Err`):
/// - `model` field missing.
/// - `prompt` field missing.
/// - `cost_budget` unit is `PerMemory` or `PerDay` (v1 supports
///   `PerRequest` only — §22/09 §5).
/// - `model_router` is `None` (no LLM clients configured).
/// - Router can't resolve the model (unknown provider OR matching
///   provider's client unset).
/// - `response_schema` fails to compile against draft-7.
///
/// True `Err` paths are reserved for genuine decode failures
/// (bad JSON blob, kind mismatch).
pub fn materialize_llm_extractor(
    def: &ExtractorDefinition,
    deps: &MaterializeDeps,
) -> Result<LlmExtractor, ExtractorError>;
```

### `build_registry_from_definitions` migration

Replaces the current `classifier_model: Option<Arc<dyn
ClassifierModel>>` param with `deps: &MaterializeDeps`. Inside,
the LLM branch becomes:

```rust
Some(ExtractorKind::Llm) => match materialize_llm_extractor(def, deps) {
    Ok(l) => registry.register(Arc::new(l)),
    Err(e) => errors.push((id, e)),
},
```

Callers of the old signature (server shard, integration tests)
get a one-line update.

## Field extraction helpers

New private helpers in `materialize.rs` (small, no public
surface):

```rust
fn extract_model(ast: &ExtractorDef) -> Option<&str>;
fn extract_prompt(ast: &ExtractorDef) -> Option<&str>;
fn extract_examples(ast: &ExtractorDef) -> Option<serde_json::Value>;
fn extract_response_schema(ast: &ExtractorDef) -> Option<serde_json::Value>;
fn extract_cost_budget_per_call(ast: &ExtractorDef)
    -> CostBudgetExtract; // enum: Unset, PerRequest(u64), Unsupported(reason)
fn extract_cache_config(ast: &ExtractorDef) -> CacheConfig;     // default Enabled
fn extract_cache_ttl(ast: &ExtractorDef) -> Option<Duration>;   // default 7d
```

`CostExpr` → micro-USD: `(amount * 1_000_000.0).round() as u64`.

`DurationAst` → `Duration`: amount * unit-multiplier.

## Degraded constructions

Each degraded path captures an operator-actionable reason so the
audit row surfaces the cause:

| Cause | Reason string |
|---|---|
| model field missing | `"llm extractor missing required 'model' field"` |
| prompt field missing | `"llm extractor missing required 'prompt' field"` |
| `model_router` None | `"no llm clients configured (set ANTHROPIC_API_KEY or OPENAI_API_KEY)"` |
| Router unresolved | `"no client configured for model X (provider Y)"` |
| Cost budget non-`PerRequest` | `"cost_budget unit Z not supported in v1 (use per_request)"` |
| Schema compile failed | `"response_schema invalid: <error>"` |

`LlmExtractor::degraded` already exists from 21.3 — reuse.

## `LlmExtractor::from_definition_fields` helper

Move the inner-state construction out of the materializer so
tests can call it without round-tripping through
`ExtractorDefinition`:

```rust
impl LlmExtractor {
    /// Build a fully-wired `LlmExtractor` from already-resolved
    /// inputs. The materializer calls this after all degraded
    /// checks pass; tests use it directly to skip the AST.
    pub fn build(
        id: ExtractorId,
        name: String,
        target: ExtractorTarget,
        extractor_version: u32,
        client: Arc<dyn LlmClient>,
        cache: Option<Arc<Mutex<LlmCacheDb>>>,
        prompt: String,
        examples: Option<Value>,
        response_schema: Option<Value>,
        schema_compiled: Option<JSONSchema>,
        confidence_threshold: f32,
        cost_budget: Option<CostBudget>,
        cache_ttl: Duration,
    ) -> Self;
}
```

This is the existing `LlmExtractor::new(..., LlmExtractorInner {
... })` re-shaped to a flatter signature. Internally still goes
through `LlmExtractorInner`.

## Tests

In `materialize.rs` tests module:

1. `materialize_llm_decodes_full_definition` — full AST blob with
   model + prompt + examples + schema + confidence_threshold +
   cost_budget(per_request) + cache(disabled) → wired
   `LlmExtractor`; assert `is_wired()` true; assert cache `None`.
2. `materialize_llm_without_router_is_degraded` — deps with
   `model_router: None`; assert `is_wired()` false; reason
   contains "no llm clients configured".
3. `materialize_llm_unknown_model_is_degraded` — router has only
   anthropic configured; AST `model: "llama-3"`; degraded with
   "no client configured for model llama-3".
4. `materialize_llm_missing_prompt_is_degraded` — AST omits
   `Prompt`; degraded with "missing required 'prompt' field".
5. `materialize_llm_missing_model_is_degraded` — AST omits
   `Model`; degraded with "missing required 'model' field".
6. `materialize_llm_bad_schema_is_degraded` — `response_schema:
   {"type": "not-a-type"}`; degraded with "response_schema
   invalid".
7. `materialize_llm_cost_budget_per_memory_is_degraded` —
   `CostExpr { unit: PerMemory }`; degraded with "cost_budget
   unit PerMemory not supported".
8. `materialize_llm_cost_budget_per_request_converts_to_micro_usd`
   — `CostExpr { amount: 0.01, unit: PerRequest }` → 10_000 µ$.
9. `materialize_llm_cache_ttl_converts_duration_ast` — 2 days →
   `Duration::from_secs(172_800)`.
10. `build_registry_routes_llm_to_real_materializer` — defs
    contain one Pattern + one Llm row; pass `MaterializeDeps`
    with a fake router → registry has 2 enabled extractors, one
    is the real LlmExtractor.
11. `build_registry_llm_without_router_registers_degraded` —
    same defs, `MaterializeDeps::default()` → registry has 2
    enabled but the LLM one is `is_wired() == false`.

Reuse the `MockClient` from `llm.rs` tests — move it to a small
`pub mod` under `#[cfg(test)]` so `materialize.rs` tests can
build a `ModelRouter` with a controllable backend. Alternatively
add a `pub(crate) mod test_support` since other tests will reuse
it.

## Out of scope

- Server-side env-var reads + `LlmCacheDb::open` at shard
  startup — phase 21.5.
- `OpsContext.llm_cache` field — phase 21.5.
- Integration tests over the wire (mock-injected via the
  registry) — phase 21.6.
- Pricing-config TOML override — post-v1.

## Single commit

`feat(extractors): 21.4 — materialise + register LLM extractors`

## Verification

```
just docker cargo test -p brain-extractors --lib materialize
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu \
    -p brain-extractors --all-targets -- -D warnings
```
