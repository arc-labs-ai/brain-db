# 19.04 Confidence Aggregation

How a statement's confidence is computed from its evidence + age. The mechanic backs the `confidence` field that every read path returns and that query routing (phase 23) ranks on.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) §"Schema" — `confidence: f32` field.
- [`./05_evidence.md`](./05_evidence.md) — evidence model the aggregation runs over.
- [`../25_provenance_versioning/00_purpose.md`](../25_provenance_versioning/00_purpose.md) — broader provenance semantics.

## 1. The formula

```
confidence(S, now) = 1 - Π (1 - c_i · decay(age_i, kind))
                    i ∈ S.evidence
```

Where:

- `c_i` ∈ `[0, 1]` — the i-th evidence entry's own confidence (set by the source: extractor, agent, human author).
- `age_i = now - evidence_i.timestamp` — how old the evidence is.
- `decay(age, kind)` ∈ `[0, 1]` — the per-kind decay function (§3).

Bounds:

- Empty evidence (`S.evidence.is_empty()`): confidence = `0.0` (no evidence, no support).
- Single evidence with `c_1 = 1.0`, no decay: confidence = `1.0`.
- Independent evidence aggregates **superlinearly** — two pieces of 0.9-confidence evidence yield `1 - (0.1 · 0.1) = 0.99`, not 0.9 + 0.9 capped.

The formula is the **noisy-OR** model: each evidence is an independent vote; the probability that **at least one** is correct is `1 minus the product of probabilities that all are wrong`.

## 2. Why noisy-OR

The substrate doesn't know whether evidence is correlated. Treating each piece independently is **conservative** — it over-attributes weight to repeated identical observations. Two extractions of the same fact from the same memory shouldn't yield 0.99 confidence; they're not independent.

Implementation cost: detect duplicates and treat as one. Phase 17 implementation does this at the evidence-add step:

```text
For each new evidence to add to S:
    if S.evidence already contains the same (memory_id, extractor_id, source_kind):
        skip   // duplicate; same vote counted once
```

After dedup, the noisy-OR holds. Cross-source overlap (different extractors confirming each other) is still treated as independent — slight optimism, accepted in v1.

## 3. Decay functions

Per-statement-kind decay reflects the different epistemic profiles:

### 3.1 Fact — slow decay

```
decay_fact(age) = exp(-age / FACT_HALF_LIFE)
```

Default `FACT_HALF_LIFE = 365 days` (~1 year). After a year, a single piece of evidence contributes half its original weight. After 2 years, a quarter. After 5 years, ~5%.

Facts are stable claims; old facts are still mostly true.

### 3.2 Preference — faster decay

```
decay_pref(age) = exp(-age / PREF_HALF_LIFE)
```

Default `PREF_HALF_LIFE = 60 days` (~2 months). Preferences change; old preferences should fade quickly so a stale extraction doesn't keep ranking high.

### 3.3 Event — no decay

```
decay_event(age) = 1.0
```

Events are point-in-time. Their evidence doesn't get less reliable with age — the moment happened and we have records. The confidence reflects how confident we are the event happened, which doesn't change.

(But: the event's *relevance* to current state may fade. That's a query-time concern, not a confidence-storage concern.)

### 3.4 Override knobs

`ConfidenceConfig` (constructed per deployment, defaults match the above):

```rust
pub struct ConfidenceConfig {
    pub fact_half_life_seconds: u64,    // default 31_536_000 (365 days)
    pub pref_half_life_seconds: u64,    // default 5_184_000  (60 days)
    pub event_decay_disabled: bool,     // default true
}
```

Phase 19's schema DSL allows per-predicate overrides (some predicates decay faster than their kind's default). v1.0 ships the kind-level defaults only.

## 4. Recomputation triggers

`confidence` is recomputed when:

| Trigger | Hot path? |
|---|---|
| `statement_create` | yes — sets initial confidence based on evidence + 0 age |
| `statement_supersede` | yes — new statement gets fresh confidence; old's stays frozen at supersession time |
| Evidence added (post-creation, phase 21 path) | yes — via `statement_add_evidence` op (not in v1.0; tracked in [`./06_open_questions.md`](./06_open_questions.md)) |
| Memory forget cascading evidence removal | yes — confidence recomputed without the removed entry |
| **Time-only** (age increases) | **no** — confidence is **not** lazily recomputed at read time |

The last point is significant. The stored `confidence` field is a **snapshot at last touch**. Read queries return that snapshot. Confidence "decay" therefore manifests via:

- The next time the statement is touched (supersede, evidence change), confidence is recomputed against `now`.
- A periodic worker (phase 21+ — "confidence sweep") iterates aging statements and refreshes.
- Query-time recomputation is opt-in via a future `recompute_at_read` flag — not in v1.0.

This trade buys speed (reads don't run the formula) at the cost of slight staleness for statements that haven't been touched in a long time.

## 5. The formula in code

`brain-core::knowledge::confidence`:

```rust
pub fn aggregate_confidence(
    evidence: &[EvidenceEntry],
    now_unix_nanos: u64,
    kind: StatementKind,
    config: &ConfidenceConfig,
) -> f32 {
    if evidence.is_empty() {
        return 0.0;
    }
    let mut product = 1.0f32;
    for e in evidence {
        let age_secs = ((now_unix_nanos.saturating_sub(e.timestamp_unix_nanos)) / 1_000_000_000) as f32;
        let decay = match kind {
            StatementKind::Event => 1.0,
            StatementKind::Fact => (-age_secs / config.fact_half_life_seconds as f32).exp(),
            StatementKind::Preference => (-age_secs / config.pref_half_life_seconds as f32).exp(),
        };
        let weighted = (e.confidence * decay).clamp(0.0, 1.0);
        product *= 1.0 - weighted;
    }
    1.0 - product
}

pub struct EvidenceEntry {
    pub memory_id: MemoryId,
    pub confidence: f32,                // [0, 1]
    pub timestamp_unix_nanos: u64,      // when the evidence was first observed
}
```

Pure function — no I/O, no state, no async. Called by `statement_ops::statement_create` (and supersede / evidence_change paths).

### 5.1 Edge cases

- Single evidence with `confidence = 0.0` → result `0.0` (decay doesn't matter).
- Single evidence with `confidence = 1.0`, age 0 → result `1.0`.
- Two evidence both 0.5, no decay → `1 - (0.5 · 0.5) = 0.75`.
- 100 evidence each 0.1, no decay → `1 - (0.9)^100 ≈ 0.9999734`. Yes, that's high; that's the noisy-OR.
- All evidence wiped by decay (`weighted ≈ 0`) → `product ≈ 1` → result `≈ 0`.
- Future timestamps (clock skew): `age_secs` saturates to 0 via `saturating_sub`; decay = 1.0.

## 6. Bucketing for indexes

`STATEMENTS_BY_PREDICATE_TABLE` uses a `confidence_bucket: u8` derived from `floor(confidence * 10).clamp(0, 10)`:

| Confidence | Bucket |
|---|---|
| 0.00 - 0.10 | 0 |
| 0.10 - 0.20 | 1 |
| ... | ... |
| 0.90 - 1.00 | 9 |
| 1.00 (boundary) | 10 |

When confidence is recomputed and the bucket changes, the index entry must be removed-from-old-bucket and inserted-to-new. `statement_ops` handles this whenever confidence changes by more than 0.05 (avoids index churn on tiny adjustments).

## 7. Confidence in queries

The default `STATEMENT_LIST` order is by confidence descending — high-confidence facts surface first. Phase 23's hybrid query router uses confidence as one input to RRF fusion alongside semantic similarity, lexical relevance, and graph proximity.

`min_confidence` filter on `STATEMENT_LIST` and `QUERY` opcodes lets callers gate on a threshold. Default threshold per-deployment, configurable via `brain.query.min_confidence`.

## 8. Tests (phase 17.9)

Phase 17.9 lands the implementation with these test cases:

- Empty evidence → 0.0.
- Single evidence c=1.0 age=0 kind=Fact → 1.0.
- Two evidence c=0.9 each, no decay → exactly `1 - (0.1)² = 0.99`.
- Fact at 1-year age (half-life=1y), c=0.9 → `0.9 · 0.5 = 0.45`, single-evidence confidence `0.45`.
- Preference at 60-day age (half-life=60d), c=0.9 → `0.45` similar.
- Event at 5-year age, c=0.9 → `0.9` (no decay).
- 100 evidence each 0.1 no decay → ≥ 0.99.
- Future timestamp clock skew → saturates to 0 age, full confidence.
- Property: confidence is monotonic in number of evidence (more evidence never decreases confidence assuming all have c ≥ 0).
- Property: confidence stays in `[0, 1]` for any input.

Test file: `crates/brain-core/src/knowledge/confidence.rs::tests` (~10 unit tests).

## 9. Open questions

See [`./06_open_questions.md`](./06_open_questions.md). Notably:

- Whether to lazily recompute at read time vs the current snapshot model.
- Whether contradicting Facts should down-weight each other (currently no — each carries its own confidence independently).
- Per-predicate decay-override interaction with kind-level defaults.
