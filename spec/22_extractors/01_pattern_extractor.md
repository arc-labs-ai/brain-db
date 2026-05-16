# 22.01 Pattern Extractor

Pattern extractors apply regex matches to memory text and emit
typed outputs (entity mentions / statements / relations). They are
the **first tier** of the §00 extraction pipeline — fast (~10–100 µs
per memory), deterministic, zero-cost, foreground-synchronous.

Cross-references:
- [`./00_purpose.md`](./00_purpose.md) — three-tier overview.
- [`./03_triggers.md`](./03_triggers.md) — when patterns run.
- [`./04_resolver.md`](./04_resolver.md) — entity resolution on match.
- [`./05_audit.md`](./05_audit.md) — audit log shape.
- [`../21_schema_dsl/02_ast.md`](../21_schema_dsl/02_ast.md) §5 —
  `ExtractorField::Patterns` AST node.

## 1. Surface

```rust
pub struct PatternExtractor {
    pub id: ExtractorId,
    pub name: String,                       // qname, e.g. "acme:person_mentions"
    pub target: ExtractorTarget,            // Entity / Statement / Relation
    pub patterns: Vec<CompiledRegex>,
    pub confidence: f32,                    // fixed per-match value
    pub trigger: TriggerExpr,
    pub depends_on: Vec<ExtractorId>,
}
```

`PatternExtractor` is constructed once at schema-apply time by
compiling each `ExtractorField::Patterns(Vec<String>)` entry. The
compiled struct is cached in the per-shard `ExtractorRegistry`.

## 2. Compilation

```rust
fn compile_patterns(raw: &[String]) -> Result<Vec<CompiledRegex>, PatternError> {
    raw.iter()
        .map(|p| CompiledRegex::new(p))
        .collect()
}
```

`CompiledRegex` wraps `regex::Regex` with a compile-time complexity
cap (DFA size) and a per-match runtime cap. Both come from the
`regex` crate's built-in `RegexBuilder::size_limit` and
`dfa_size_limit` settings.

**Caps (v1, conservative):**
- DFA size limit: 1 MiB per pattern.
- NFA size limit: 1 MiB per pattern.
- Match-time backtracking budget: 10 000 steps per pattern per text.

Patterns that exceed any cap fail compilation with `PatternError::
ResourceLimit`. The extractor never registers.

## 3. Execution

```rust
fn run(&self, mem: &Memory) -> Vec<ExtractedItem> {
    let mut out = Vec::new();
    for r in &self.patterns {
        for cap in r.captures_iter(&mem.text) {
            let text = cap_text(&cap);
            out.push(self.project(mem, &text, cap.range()));
        }
    }
    out
}
```

Per-extractor invariants:
- Walks all patterns in source order.
- For each match, emits exactly one `ExtractedItem`. Overlap is
  allowed (one extractor may produce overlapping mentions from
  different patterns).
- Captures use the first capture group if any; else the full match.
- `range` is byte-offset into `mem.text`; UTF-8-safe.

## 4. Output projection

Per the `target` enum from §21/02 §5:

| `ExtractorTarget` | Output kind |
|---|---|
| `Entity { entity_type }` | `EntityMention { entity_type, text, range }` — picked up by the entity-resolver worker (§22/04). |
| `Statement { kind }` | `StatementMention { kind, text, range, confidence }` — phase 22+ promotes to a full `Statement` via a follow-on extractor or resolver. |
| `Relation { relation_type }` | Requires two capture groups (`subject`, `object`); emits `RelationMention { relation_type, subject_text, object_text, confidence }`. |
| `EntityOrStatement` | Best-effort — emits `EntityMention` if the match looks like a name, else `StatementMention`. v1 always emits `EntityMention`. |

`ExtractedItem` is the sum type carrying any of the above plus
provenance fields (`extractor_id`, `extracted_at_unix_nanos`,
`schema_version`).

## 5. Confidence

Pattern extractors **don't compute per-match confidence**. Every
emitted item carries `ExtractorDef.fields[Confidence(_)]` verbatim
(default `0.7` if the user omits the field, per §22/00).

The resolver tier downstream may multiply this by its own
confidence (e.g., low-confidence resolution drops the overall
score). The audit record retains both values.

## 6. Determinism

Pattern execution is bit-deterministic given:
- The same compiled `regex::Regex` (regex crate is deterministic).
- The same `mem.text` bytes.
- The same source-order of patterns.

The `regex` crate version is pinned at the workspace level (see
[`../06_ann_index/`](../06_ann_index/) prior pin practice).
Upgrading the crate is a versioned event: every extractor that
relies on a particular regex feature gets a `schema_version` bump
on rebuild.

## 7. Errors

```rust
pub enum PatternError {
    InvalidRegex { index: usize, message: String },
    ResourceLimit { index: usize, limit: &'static str },
    EmptyPatterns,
}
```

- `InvalidRegex` — surfaces during `compile_patterns`; aborts the
  schema upload with `ExtractorInvalidConfig` (§21/03 §2.7).
- `ResourceLimit` — same; the regex is too large to compile.
- `EmptyPatterns` — `pattern` extractor with no `patterns:` field;
  the validator already catches this in §21/03 §2.7 but the
  compiler asserts it defensively.

## 8. Performance budget

Spec §16/02 §2.6 (extended in phase 20.0):

| Operation | p50 | p99 |
|---|---|---|
| `PatternExtractor::run` over a 4 KiB memory | 30 µs | 100 µs |

The bench in phase 20.10 runs N patterns × M memories and asserts
the p99 stays under cap.

## 9. Idempotency

Re-running the same `(memory_id, text_hash, extractor_id,
extractor_version)` produces byte-identical outputs. The audit
record's `input_hash` field carries the BLAKE3 of `mem.text`; a
re-run with matching hash + extractor version writes a duplicate
audit row only if the caller explicitly requested replay (§22/06).
