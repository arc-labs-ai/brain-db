# Phase 20 — Pattern + classifier extractors

Extractor framework + pattern + classifier + built-in
`brain.entity_mentions` (pattern) and `brain.basic_ner` (classifier).
LLM extractors deferred to phase 21.

## Confirmed scope (user decisions, 2026-05-16)

1. **Full classifier tier with bundled NER model.** Phase 20 ships
   pattern *and* classifier, with a small bundled NER model for
   `brain.basic_ner`. No LLM (phase 21).
2. **Schema-driven extractor registry.** The §19.7
   `apply_schema_definitions` path's currently-skipped
   `SchemaItem::Extractor` arm gets fleshed out: user
   `define extractor` blocks register into the extractors table at
   `schema_upload` time. Built-ins seed via the same path through
   the system schema.
3. **New crate `brain-extractors`** per spec §08.

## Prerequisites

- Phase 19 complete (`phase-19-complete` at `bcb9652`; merged into
  `dev` at `3f98a74`, into `main` at `4312bc4`).
- Branch off `dev`: `feature/phase-20-extractors`.

## Branch

`feature/phase-20-extractors` (created off `dev`).

## Spec-first discipline — §22 / §27 / §25 backfill required

Current state of relevant sections:

- `spec/22_extractors/`        — 1 file (`00_purpose.md`).
- `spec/27_knowledge_workers/` — 1 file (`00_purpose.md`).
- `spec/25_provenance_versioning/` — 1 file (`00_purpose.md`).

Per memory `feedback_spec_first_workflow` these must be brought to
§03-depth (matching §19 / §20 / §21's 8-file shape) before any code
lands. Sub-task 20.0 is doc-only.

## Sub-tasks (provisional cadence)

### 20.0 — §22 + §27 + §25 spec backfill

**Reads:** §22/00, §27/00, §25/00, §21/06 (system schema), §28/05
(schema wire) §6 (`EXTRACTOR_LIST`), §28/05 §7 (`EXTRACTOR_DISABLE`
/ `_ENABLE`).
**Writes (~21 spec files):**

- `spec/22_extractors/01_pattern_extractor.md` — regex sources,
  compilation, match → output projection, confidence semantics.
- `spec/22_extractors/02_classifier_extractor.md` — model interface,
  feature extraction, determinism contract, inference budget.
- `spec/22_extractors/03_triggers.md` — `on encode` / `on demand` /
  `on schema_change` / `periodic`, condition_expr evaluation.
- `spec/22_extractors/04_resolver.md` — `on_match: resolve_entity`
  semantics, dedupe / supersession.
- `spec/22_extractors/05_audit.md` — `ExtractionAudit` shape +
  retention.
- `spec/22_extractors/06_idempotency.md` — input hash, cache key,
  replay semantics.
- `spec/22_extractors/07_open_questions.md`.
- `spec/22_extractors/08_references.md`.
- `spec/27_knowledge_workers/01_extractor_workers.md` — worker loop,
  scheduler, queue.
- `spec/27_knowledge_workers/02_decay_worker.md` (placeholder; phase 22+).
- `spec/27_knowledge_workers/03_resolution_workers.md` (placeholder).
- `spec/27_knowledge_workers/07_open_questions.md`.
- `spec/27_knowledge_workers/08_references.md`.
- `spec/25_provenance_versioning/01_audit_tables.md` —
  `extractor_audit` + `entity_resolution_audit` redb shapes.
- `spec/25_provenance_versioning/02_event_provenance.md`.
- `spec/25_provenance_versioning/07_open_questions.md`.
- `spec/25_provenance_versioning/08_references.md`.

Bundled edits:

- `spec/16_benchmarks_acceptance/02_latency_targets.md` — add
  extractor perf targets (pattern ≤100 µs, classifier ≤10 ms, audit
  write ≤1 ms) + phase-gate renumber.
- `spec/21_schema_dsl/07_open_questions.md` — resolve Q-on-extractor
  fan-out (was deferred in 19.7).

**Done when:** §22 mirrors §19 / §20 / §21 8-file depth.

### 20.1 — Extractor trait + registry types

**Writes:** `crates/brain-extractors/` (new crate) — `lib.rs`,
`Extractor` trait, `ExtractedItem` enum (Entity / Statement /
Relation / EntityMention), `ExtractorRegistry`,
`ExtractionContext`, `ExtractionResult`.
**Done when:** trait is object-safe (`dyn Extractor`); registry
hands out `&dyn Extractor`s by `ExtractorId`.

### 20.2 — Pattern extractor

**Writes:** `crates/brain-extractors/src/pattern.rs`.
**Done when:** `PatternExtractor` compiles regexes from
`ExtractorField::Patterns`, runs them over memory text, emits
`EntityMention` outputs (linked to the target entity type for
`resolve_entity` flow).
**Pitfalls:** Precompile via `regex` crate (already a workspace dep
in §06 stack? — verify). Cap per-pattern runtime via the regex
crate's match-budget setting.

### 20.3 — Classifier framework + bundled NER

**Writes:** `crates/brain-extractors/src/classifier.rs` + bundled
model assets in `crates/brain-extractors/models/`.
**Done when:** small CONLL-trained NER model runs over memory text
via candle (CPU); produces PER / ORG / LOC tags; pinned weights +
tokenizer + seed for determinism.
**Pitfalls:** Model size — aim ≤ 30 MB compressed; `include_bytes!`
or `models/` dir + runtime load.
**Risk:** Model availability — fall back to a synthetic deterministic
classifier (e.g., rule-based stub disguised as a classifier) if a
clean ~30 MB CONLL-NER checkpoint isn't readily licensable in the
candle format. Flag this as a 20.3 sub-decision when implementing.

### 20.4 — Audit log

**Writes:** `crates/brain-metadata/src/audit_ops.rs` +
`tables/knowledge/audit.rs` (widen `extractor_audit` row to match
spec §22/05).
**Done when:** every extraction call writes one
`ExtractionAuditRow` (`SUCCESS` / `FAILURE` / `SKIPPED_BUDGET` /
`SKIPPED_FILTER`); admin query helpers `audit_by_memory`,
`audit_by_extractor`, `audit_recent_failures`.

### 20.5 — Extractor registry persistence (schema fan-out)

**Writes:** `crates/brain-metadata/src/extractor_ops.rs` +
extend 19.7's `apply_schema_definitions` to handle
`SchemaItem::Extractor`.
**Done when:**
- User-declared extractors land in the `extractors` redb table.
- The §22-spec'd `ExtractorDefinition` row carries the kind,
  target, fields blob, schema_version stamping.
- Idempotent on re-upload of identical definitions.

### 20.6 — ENCODE handler integration

**Writes:** extend `crates/brain-ops/src/ops/encode.rs` to drive
the extractor pipeline post-commit: pattern extractors run
synchronously; classifier extractors run synchronously when budget
allows; LLM extractors deferred (phase 21).
**Done when:**
- ENCODE returns within the existing latency budget (P99 ≤ 20 ms
  per spec §16/02 §2.1).
- Pattern + classifier outputs are visible to subsequent RECALL /
  entity-list calls.
- Failed extractions don't fail ENCODE (logged + audit only).

### 20.7 — Built-in `brain.entity_mentions` pattern + system schema update

**Writes:** extend `crates/brain-metadata/src/system_schema/schema.brain`
to declare `brain.entity_mentions` and `brain.basic_ner`. Add the
built-in extractor logic in `brain-extractors`.
**Done when:** system schema loads two built-in extractors at
`MetadataDb::open`; ENCODE smokes a Person mention end-to-end.

### 20.8 — Wire opcodes 0x0124-0x0126

**Writes:** `crates/brain-protocol/src/knowledge/extractor_req.rs`
+ `_resp.rs`; handlers in `brain-ops/src/ops/knowledge_extractor.rs`.
**Done when:** `EXTRACTOR_LIST` / `_DISABLE` / `_ENABLE` work over
the wire per §28/05 §6-§7.

### 20.9 — Integration tests

**Writes:**
- `crates/brain-server/tests/knowledge_extractor_wire.rs` — wire
  smoke for 3 extractor opcodes.
- `crates/brain-server/tests/knowledge_extractor_pattern_classifier.rs` —
  end-to-end ENCODE → extraction → entity mention surfaces in
  audit + queryable via RECALL.

### 20.10 — Bench + ROADMAP + phase exit

**Writes:**
- `crates/brain-extractors/benches/pattern_extract.rs` +
  `classifier_extract.rs`.
- ROADMAP phase 20 ✓.
- User-authorised tag `phase-20-complete`.

## Risks

- **Bundled model availability.** Open question whether a
  satisfactorily-small (≤30 MB), candle-compatible, permissively-
  licensed CONLL-NER checkpoint exists. Sub-task 20.3 makes the
  call at implementation time; fallback is a deterministic
  rule-based classifier.
- **ENCODE latency budget.** Adding synchronous extraction to
  ENCODE is risk for P99. Bench gate at end of 20.6.
- **Spec backfill weight.** §22 / §27 / §25 backfill (20.0) is
  ~21 spec files; comparable to phase 19's §21 backfill (9 files
  + bundled edits).

## Suggested commit cadence

- `20.0` — spec backfill (single doc-only commit).
- `20.1-20.10` — one commit each.

11 commits total, matching phase 19's cadence.

## Verification gate (per sub-task)

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-extractors
just docker cargo test -p brain-metadata -p brain-server
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```

## After phase 20

Phase 21 — LLM extractor. The schema DSL gives LLM extractors a
declarative target; the cache layer slots in alongside `brain-llm`.
