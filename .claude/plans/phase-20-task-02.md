# 20.2 — Pattern extractor

`PatternExtractor: Extractor` implementation. Compiles regexes
from `ExtractorField::Patterns` once at schema-apply time, runs
them over `memory.text`, emits typed mentions per the `target`.

Per spec §22/01.

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-extractors/src/pattern.rs` | `PatternExtractor` + `CompiledRegex` + `compile_patterns` + projection helpers. |
| `crates/brain-extractors/src/lib.rs` | Add `pub mod pattern;` + re-exports. |
| `crates/brain-extractors/Cargo.toml` | Add `regex` workspace dep. |
| `Cargo.toml` (root) | Add `regex = "1"` to `[workspace.dependencies]`. |

## Public surface

```rust
pub struct PatternExtractor {
    id: ExtractorId,
    name: String,
    target: ExtractorTarget,
    extractor_version: u32,
    patterns: Vec<CompiledRegex>,
    confidence: f32,
}

pub struct CompiledRegex {
    raw: String,
    re: Regex,
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
    ) -> Result<Self, ExtractorError>;

    /// Source patterns (for debugging / round-trip).
    pub fn patterns(&self) -> &[CompiledRegex];

    /// Fixed per-match confidence (§22/01 §5).
    pub fn confidence(&self) -> f32;
}

impl Extractor for PatternExtractor { ... }
```

## Compilation caps

Per spec §22/01 §2 — pinned conservative caps:

```rust
const DFA_SIZE_LIMIT: usize = 1 << 20;  // 1 MiB
const NFA_SIZE_LIMIT: usize = 1 << 20;
```

The `regex` crate's `RegexBuilder::size_limit` covers both NFA and
DFA jointly. Phase 20 sets a single 1 MiB limit; bigger patterns
fail with `ResourceLimit { index, limit: "regex compile size" }`.

Match-time backtracking budget is harder to set programmatically
on regex 1.x — the crate uses linear-time DFA matching for most
patterns. v1 leaves it at the default and tracks "per-pattern
runtime cap" as a §22/07 follow-up if benchmarks flag pathological
patterns.

## Execution

```rust
fn run(&self, ctx: &ExtractionContext<'_>, mem: &Memory) -> ExtractionResult {
    let start = ctx.now_unix_nanos;
    let mut items = Vec::new();
    for r in &self.patterns {
        for m in r.re.find_iter(&mem.text) {
            // First capture group if present, else the full match.
            let (text, start_off, end_off) = if let Some(caps) = r.re.captures(&mem.text[m.range()]) {
                if let Some(c) = caps.get(1) {
                    let abs_start = m.start() + c.start();
                    let abs_end = m.start() + c.end();
                    (c.as_str().to_string(), abs_start, abs_end)
                } else {
                    (m.as_str().to_string(), m.start(), m.end())
                }
            } else {
                (m.as_str().to_string(), m.start(), m.end())
            };
            items.push(self.project(text, start_off, end_off));
        }
    }
    let end = ctx.now_unix_nanos;  // caller bumps it; the trait doesn't force monotonic clock here
    ExtractionResult::success(items, start, end)
}
```

Projection per target (§22/01 §4):

- `Entity { entity_type }` → `EntityMention { entity_type_qname: entity_type.clone(), text, start, end, confidence, extractor_id, extractor_version }`.
- `Statement { kind }` → `StatementMention { kind: discriminant(kind), subject_text: None, predicate_qname: "<unknown>", object_text: Some(text), confidence, ... }`. Predicate qname is `""` in v1 — pattern extractors that target statements need a downstream resolver to pick the predicate; phase 20 doesn't ship one (see §22/07 Q6).
- `Relation { relation_type }` — requires **two capture groups**; emits `RelationMention { subject_text: cap[1], object_text: cap[2], ... }`. If only one group is captured, the match is skipped (silently — no audit-side surfacing in v1; tracked in §22/07).
- `EntityOrStatement` → emits `EntityMention` with `entity_type_qname = ""` and lets the resolver decide.

## Tests

Unit tests in `pattern.rs`:

1. `try_new_compiles_simple_patterns` — basic regex compiles.
2. `try_new_rejects_invalid_regex` — `[a-` → `RegexCompile`.
3. `try_new_rejects_oversized_pattern` — a pathological alternation > 1 MiB → `ResourceLimit`.
4. `try_new_rejects_empty_patterns` — `[]` → `EmptyPatterns`.
5. `run_emits_entity_mention_for_each_match` — `\bAlice\b` over `"Alice met Alice"` → 2 mentions, offsets correct.
6. `run_uses_first_capture_group_when_present` — `\b([A-Z]\.\s[A-Z][a-z]+)\b` extracts the group, not the whole match.
7. `run_with_no_matches_returns_empty_items_and_success` — non-matching pattern → `Success` status, zero items.
8. `run_for_relation_target_requires_two_groups` — single-group pattern with relation target → 0 items.
9. `run_for_relation_target_emits_subject_object_from_groups` — `(\w+) reports to (\w+)` over `"Bob reports to Priya"` → 1 RelationMention with subject="Bob", object="Priya".
10. `confidence_propagates_to_emitted_items` — `confidence(0.42)` → all items carry 0.42.
11. `extractor_id_and_version_stamped_on_outputs` — items carry the trait-level metadata.
12. `unicode_offsets_are_byte_safe` — multi-byte UTF-8 match offsets land on character boundaries.

## Integration via existing crate-level tests

`brain-extractors` crate's lib-level test run grows from 20 → 32
roughly (pattern tests add ~12).

## Out of scope

- Audit-row writes — 20.4.
- Resolver tier (mention → persisted entity) — 20.6 / phase 22.
- `Statement` target predicate inference — §22/07 Q6.

## Single commit

`feat(extractors): 20.2 — pattern extractor`

## Verification

```
cargo test -p brain-extractors
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
