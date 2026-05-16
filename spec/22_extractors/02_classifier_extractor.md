# 22.02 Classifier Extractor

Classifier extractors run a pinned, deterministic model over
memory text + features and emit typed outputs. They are the
**second tier** of the §00 pipeline — slower than patterns
(~1–10 ms per memory) but precise on inputs patterns can't catch.

Cross-references:
- [`./01_pattern_extractor.md`](./01_pattern_extractor.md) — tier 1.
- [`./03_triggers.md`](./03_triggers.md) — `confidence_threshold`
  + `trigger` fields.
- [`../27_knowledge_workers/01_extractor_workers.md`](../27_knowledge_workers/01_extractor_workers.md)
  — near-foreground scheduling.

## 1. Surface

```rust
pub struct ClassifierExtractor {
    pub id: ExtractorId,
    pub name: String,
    pub target: ExtractorTarget,
    pub model: Box<dyn ClassifierModel>,
    pub feature_extractor: FeatureExtractor,
    pub confidence_threshold: f32,
    pub trigger: TriggerExpr,
    pub depends_on: Vec<ExtractorId>,
}
```

`ClassifierModel` is an object-safe trait:

```rust
pub trait ClassifierModel: Send + Sync {
    fn predict(&self, features: &Features) -> Prediction;
    fn version(&self) -> &str;          // pinned; e.g. "brain-basic-ner-v1.0"
}
```

`Features` is an opaque newtype carrying whatever
`feature_extractor` produces — typically tokenised text + optional
NER tags from a preceding pattern pass.

## 2. Determinism contract

The classifier MUST be bit-deterministic across runs of the same
binary version:

| Source | Pinning |
|---|---|
| Model weights | Embedded via `include_bytes!` or shipped at a fixed `models/` path. |
| Tokeniser | Pinned (same crate version, same vocabulary file). |
| Random seed | Fixed (0). |
| Math library | Pinned (candle workspace version). |
| Float ops | Default precision; no opt-in fast-math. |

A change to any of the above is a **model version bump** —
`ClassifierModel::version()` returns a new string, every
`ExtractionAudit` row written after the bump carries the new
version, and downstream statements get a `schema_version` /
`extractor_version` bump that the stale-extraction detector
(§25/00) notices.

## 3. Feature extraction

`FeatureExtractor` is one of:

- `Builtin` — uses the model's bundled tokeniser + featurizer.
  Standard path; v1 phase-20 uses this for `brain.basic_ner`.
- `Custom { id: FeatureExtractorId }` — refers to a registered
  Rust function. Out of scope for v1 phase 20; tracked in
  [`./07_open_questions.md`](./07_open_questions.md).

```rust
pub enum FeatureExtractor {
    Builtin,
    Custom { id: FeatureExtractorId },
}
```

## 4. Execution

```rust
fn run(&self, mem: &Memory) -> Vec<ExtractedItem> {
    let features = self.feature_extractor.extract(mem);
    let pred = self.model.predict(&features);
    if pred.confidence < self.confidence_threshold {
        return vec![];
    }
    vec![self.project(mem, pred)]
}
```

Output projection mirrors §22/01 §4:

| `ExtractorTarget` | Output kind |
|---|---|
| `Entity { entity_type }` | One `EntityMention` per detected span; predictions with multiple spans emit multiple items. |
| `Statement { kind }` | One `StatementMention` per high-confidence prediction. |
| `Relation { relation_type }` | One `RelationMention` per (subject, object) pair the model emits. |

`Prediction.confidence` carries through to `ExtractedItem.confidence`
verbatim — unlike pattern extractors, classifiers can produce
variable per-match confidence, and the value lands in the audit
row.

## 5. Performance budget

Spec §16/02 §2.6:

| Operation | p50 | p99 |
|---|---|---|
| `ClassifierExtractor::run` over a 4 KiB memory | 5 ms | 15 ms |

Budget includes feature extraction + inference. ENCODE's overall
P99 budget (§16/02 §2.1) absorbs at most one classifier extractor
per memory; multiple classifiers go to a near-foreground queue
(§27/01).

## 6. Built-in `brain.basic_ner`

Phase 20.7 ships one built-in classifier:

```text
define extractor brain.basic_ner {
    kind: classifier
    target: entity Person
    model: "brain-basic-ner-v1"
    feature_extraction: builtin
    confidence_threshold: 0.6
    trigger: on encode
}
```

Model details:
- Architecture: small distilled BERT (or fallback rule-based; see
  phase 20.3 risk note).
- Weights: ≤ 30 MB compressed, bundled via `include_bytes!` in
  `crates/brain-extractors/`.
- Output classes: `PER`, `ORG`, `LOC`, `O`.
- v1 projects only `PER` spans into `EntityMention { entity_type:
  Person }`; `ORG` / `LOC` are dropped until phase 22+ adds
  `Organization` / `Location` to the system schema.

## 7. Errors

```rust
pub enum ClassifierError {
    ModelNotFound { id: String },
    FeatureExtractionFailed { reason: String },
    InferenceFailed { reason: String },
    OutputDecodeFailed { reason: String },
}
```

Classifier errors are captured in the audit row's `status =
Failure` + `error: Some(_)`. They DO NOT fail the surrounding
ENCODE; the extractor returns empty output and the audit row
records the failure.

## 8. Open questions

See [`./07_open_questions.md`](./07_open_questions.md). Notably:

- Q-classifier-model — exact CONLL-NER checkpoint, licensing,
  candle compatibility (phase 20.3 risk).
- Q-batching — multi-memory batching (phase 22+).
- Q-feature-extractor-custom — user-supplied feature extractors
  (post-v1).
