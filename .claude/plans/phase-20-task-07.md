# 20.7 — Built-in extractors + system schema integration

Lands `brain.entity_mentions` (pattern) and `brain.basic_ner`
(classifier) declarations in the system schema. Wires registry
materialisation at shard startup so the runtime
`ExtractorRegistry` is populated from `EXTRACTORS_TABLE` on every
boot.

## Scope — pattern works end-to-end; classifier stays degraded

BertRuntime candle wiring is **deferred to 20.7b** (~300 LOC of
candle math that needs operator-provided weights to verify).
Phase 20.7 ships:

- Pattern extractor `brain.entity_mentions` end-to-end through
  ENCODE.
- Classifier `brain.basic_ner` registered and dispatched but
  returning the staged `Failure(reason: "runtime not wired")` —
  the framework / audit / dispatch path is exercised; only the
  inference math is missing.
- `BRAIN_NER_MODEL_PATH` env knob wired through OpsContext so
  20.7b's BertRuntime impl picks it up without code churn.

## Files written / modified

| Path | Change |
|---|---|
| `crates/brain-metadata/src/system_schema/schema.brain` | Add `brain.entity_mentions` pattern + `brain.basic_ner` classifier blocks. |
| `crates/brain-server/src/shard/mod.rs` | After `MetadataDb::open`, materialise the registry via `build_registry_from_definitions` and pass to `OpsContext::with_extractor_registry`. Read `BRAIN_NER_MODEL_PATH` env → `with_classifier_config`. |
| `crates/brain-server/Cargo.toml` | Add `brain-extractors` dep. |
| `crates/brain-metadata/src/system_schema/mod.rs` | Update tests to expect the new built-in extractor rows. |

## System schema additions

Appended to `schema.brain`:

```text
# --- Built-in extractors -----------------------------------------

define extractor entity_mentions {
    kind: pattern
    target: entity Person
    patterns [
        # English-style person names: First Last; F. Last; First M. Last.
        /\b([A-Z][a-z]+\s+[A-Z][a-z]+)\b/,
        /\b([A-Z]\.\s*[A-Z][a-z]+)\b/
    ]
    confidence: 0.7
}

define extractor basic_ner {
    kind: classifier
    target: entity Person
    model: "brain-basic-ner-v1"
    feature_extraction: builtin
    confidence_threshold: 0.6
    trigger: on encode
}
```

These get assigned `ExtractorId(1)` (entity_mentions) and
`ExtractorId(2)` (basic_ner) on first boot, stable across
re-opens.

The `feature_extraction: builtin` field requires a grammar tweak
to accept the bare identifier `builtin` per §21/01 §4 — already
in the grammar (`kw_builtin = @{ "builtin" }`).

## Registry materialisation at shard startup

```rust
// Inside spawn_shard's async closure, after `metadata` is built:

let extractor_registry = {
    let rtxn = metadata.lock().read_txn()
        .expect("read_txn after MetadataDb::open");
    let defs = brain_metadata::extractor_list(&rtxn)
        .expect("extractor_list");
    drop(rtxn);

    // Classifier model is unwired in phase 20.7; phase 20.7b
    // loads it from BRAIN_NER_MODEL_PATH.
    let (reg, errors) =
        brain_extractors::build_registry_from_definitions(&defs, None);
    for (id, err) in errors {
        tracing::warn!(
            target: "brain_server::shard",
            extractor_id = id.raw(),
            error = %err,
            "extractor materialise failed; skipping",
        );
    }
    reg
};

let classifier_config = match std::env::var("BRAIN_NER_MODEL_PATH") {
    Ok(p) => brain_extractors::ClassifierConfig::with_model_path(p.into()),
    Err(_) => brain_extractors::ClassifierConfig::unloaded(),
};

let ops = Arc::new(
    OpsContext::new(executor_ctx)
        .with_extractor_registry(extractor_registry)
        .with_classifier_config(classifier_config),
);
```

## Tests

### `system_schema/mod.rs` updates

Existing tests gain assertions that the new built-in extractor
rows appear in `EXTRACTORS_TABLE` after `seed_system_schema`:

- `seed_first_open_creates_brain_v1` → also assert
  `extractor_list(rtxn).len() == 2`.
- `seed_reopen_is_idempotent` → also assert the count stays at 2.
- New `system_schema_extractor_ids_are_stable`: assert
  `brain.entity_mentions` → id 1 and `brain.basic_ner` → id 2 on
  first open.
- New `system_schema_extractor_definitions_decode`: round-trip
  `definition_blob` → `ExtractorDef` via serde_json.

### `shard/mod.rs` smoke

The existing `spawn_unbound_and_join` test exercises the spawn
path; we add an assertion at the end that the spawned shard's
`OpsContext` has a non-empty extractor registry by enqueuing one
ENCODE and verifying an audit row appears for the pattern
extractor.

Skipped if cross-runtime test wiring is too heavy — relying on
the unit-level tests in §22 for full coverage.

## Out of scope (→ 20.7b)

- BertRuntime candle forward pass.
- Real classifier inference.
- Cross-shard model sharing.

## Single commit

`feat(server): 20.7 — built-in extractors via system schema + shard registry materialisation`

## Verification

```
just docker cargo test -p brain-metadata --lib system_schema
just docker cargo test -p brain-extractors --lib materialize
just docker cargo test -p brain-server --test '*' shard
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
