# 28.11 Knowledge-Layer Wire Validation

Per-field validation rules applied **before** a knowledge request reaches its handler. Mirrors the substrate's [`../03_wire_protocol/11_validation.md`](../03_wire_protocol/11_validation.md): the server fails-fast on malformed input with a structured ERROR rather than risking a partial write.

Validation lives in two layers:

1. **Wire-layer** — rkyv structural validation (`check_bytes`) catches malformed archives. Returns `MalformedRkyv`.
2. **Handler-layer** — semantic validation (length caps, type-id existence, etc.) before any storage call. Returns the appropriate §28 error code.

This file specifies the **handler-layer** rules.

## 1. Universal field caps

Applied to every string / blob field across the knowledge namespace. Limits chosen to balance correctness (catch obvious bugs) with permissiveness (don't reject legitimate Unicode-heavy text).

| Field shape | Max size | Source |
|---|---|---|
| Identifier-ish string (e.g. `canonical_name`, `predicate_name`, `schema_version`) | 256 bytes UTF-8 | §28 (this file) |
| Free-form text (e.g. `reason`, `context`, `message`) | 4096 bytes UTF-8 | §28 |
| Opaque blob (e.g. `attributes_blob`, `evidence_blob`) | 64 KiB | §28 |
| Collection (e.g. `aliases`, `candidate_ids`) | 256 elements | §28 |
| Cursor / pagination token | 1 KiB | §28 |

Violations → `INVALID_ARGUMENT` (substrate `Validation` category) with `details.field` naming the offender and `details.expected` carrying the limit.

## 2. Entity opcode rules (`0x0130–0x0138`)

### 2.1 ENTITY_CREATE (`0x0130`)

| Field | Rule | Error code if violated |
|---|---|---|
| `entity_type_id` | must be > 0 and registered in `entity_types` table | `ENTITY_TYPE_MISMATCH` |
| `canonical_name` | non-empty after `.trim()`; ≤ 256 bytes; valid UTF-8 (rkyv guarantees) | `INVALID_ARGUMENT` |
| `canonical_name` (after server-side `normalize_name`) | must not collide with an existing entity of the same `entity_type_id` | `ENTITY_AMBIGUOUS` (duplicate) |
| `aliases.len()` | ≤ 32 (per [§18/00](../18_entities/00_purpose.md) §"Field semantics") | `INVALID_ARGUMENT` |
| each `alias` | non-empty after `.trim()`; ≤ 256 bytes | `INVALID_ARGUMENT` |
| `attributes_blob` | ≤ 64 KiB | `INVALID_ARGUMENT` |
| `request_id` | non-zero UUIDv7 | `INVALID_ARGUMENT` |

Aliases are deduplicated server-side on the normalized form before insertion. A request supplying duplicates is **not** rejected — duplicates are silently collapsed.

### 2.2 ENTITY_GET (`0x0131`)

| Field | Rule | Error code |
|---|---|---|
| `entity_id` | non-zero UUIDv7 | `INVALID_ARGUMENT` |

No further validation — missing rows return `ENTITY_NOT_FOUND` from the handler.

### 2.3 ENTITY_UPDATE (`0x0132`)

Same field-level caps as `ENTITY_CREATE`, plus:

- `entity_id` must be non-zero and exist (→ `ENTITY_NOT_FOUND` otherwise).
- If `canonical_name` differs from the current row's *normalized* form, treat as an implicit rename and apply rename validation (§2.4).
- `entity_type_id` is **ignored** (`ENTITY_UPDATE` cannot retype). A future opcode handles retypes; the wire field is reserved for forward compat. Currently the handler reads the field for validation parity but discards it before writing.

### 2.4 ENTITY_RENAME (`0x0133`)

| Field | Rule | Error code |
|---|---|---|
| `entity_id` | non-zero; entity must exist; entity must not be tombstoned | `ENTITY_NOT_FOUND` |
| `new_canonical_name` | non-empty; ≤ 256 bytes | `INVALID_ARGUMENT` |
| `new_canonical_name` (normalized) | must not collide with an existing entity of the same `entity_type_id` | `ENTITY_AMBIGUOUS` |
| `move_to_alias` | currently must be `true` (phase 16.6c constraint) | `INVALID_ARGUMENT` |

### 2.5 ENTITY_MERGE (`0x0134`)

| Field | Rule | Error code |
|---|---|---|
| `survivor`, `merged` | both non-zero; both exist; both same `entity_type_id` (cross-type merge forbidden in v1) | `ENTITY_NOT_FOUND` / `ENTITY_TYPE_MISMATCH` |
| `survivor == merged` | rejected | `ENTITY_MERGE_CONFLICT` |
| `merged.merged_into` | must be `None` (no double-merge) | `ENTITY_MERGE_CONFLICT` |
| `survivor.merged_into` | must be `None` (survivor is itself active) | `ENTITY_MERGE_CONFLICT` |
| `confidence` | in `[0.0, 1.0]`; finite | `INVALID_ARGUMENT` |
| `confidence` ≥ 0.7 | otherwise rejected (merge candidates require ≥ 0.7; see [§18/00](../18_entities/00_purpose.md) §"Merging entities") | `INVALID_ARGUMENT` |
| `reason` | ≤ 4096 bytes | `INVALID_ARGUMENT` |

### 2.6 ENTITY_UNMERGE (`0x0135`)

| Field | Rule | Error code |
|---|---|---|
| `merged_entity` | non-zero; must exist | `ENTITY_NOT_FOUND` |
| `merged_entity.merged_into` | must be `Some(_)` | `ENTITY_NOT_FOUND` (interpretable as "no merge to unmerge") |
| merge audit `created_at + grace_period` | must be > now | `ENTITY_MERGE_CONFLICT` |

### 2.7 ENTITY_RESOLVE (`0x0136`)

| Field | Rule | Error code |
|---|---|---|
| `candidate_name` | non-empty after `.trim()`; ≤ 256 bytes | `INVALID_ARGUMENT` |
| `context` | ≤ 4096 bytes; handler truncates to first 100 chars before passing to resolver | `INVALID_ARGUMENT` |
| `entity_type_hint` | `0` allowed (no hint); otherwise must be registered | `ENTITY_TYPE_MISMATCH` |
| schema declared? | required | `SchemaNotDeclared` |

### 2.8 ENTITY_LIST (`0x0137`)

| Field | Rule | Error code |
|---|---|---|
| `entity_type_id` | `0` (no filter) or registered | `ENTITY_TYPE_MISMATCH` |
| `name_prefix` | ≤ 256 bytes; server normalizes before prefix-matching | `INVALID_ARGUMENT` |
| `limit` | 1 ≤ limit ≤ 1000 | `INVALID_ARGUMENT` |
| `cursor` | ≤ 1 KiB; server-defined opaque shape; malformed → reject | `INVALID_ARGUMENT` |

### 2.9 ENTITY_TOMBSTONE (`0x0138`)

| Field | Rule | Error code |
|---|---|---|
| `entity_id` | non-zero; must exist; already-tombstoned returns success (idempotent) | `ENTITY_NOT_FOUND` |
| `reason` | ≤ 4096 bytes | `INVALID_ARGUMENT` |

## 3. Schema, statement, relation, query, admin (spec-only here)

Each later phase fills in the corresponding section. Placeholder rules to anchor the discipline:

### 3.1 Schema ops (0x0120–0x0126)

- `schema_document` (`SCHEMA_UPLOAD` / `SCHEMA_VALIDATE`): ≤ 1 MiB (raised from the universal 64 KiB cap; schema documents are intentionally larger).
- `version_id` (`SCHEMA_GET`): `0` means "latest", otherwise must exist → `SCHEMA_INVALID`.
- `extractor_id`: must be in the active extractor registry → `INVALID_ARGUMENT` otherwise.

### 3.2 Statement ops (0x0140–0x0146)

- `subject`, `object` (when `EntityRef`): must resolve to existing entity → `ENTITY_NOT_FOUND`.
- `predicate`: a `"namespace:name"` qname. Open-vocabulary in schemaless mode (interned on first use with `SchemaOrigin::ImplicitFromWrite`); strict mode rejects unknown qnames with `PredicateNotInSchema` (0x004B). Declared object-type constraints → `STATEMENT_OBJECT_TYPE_MISMATCH`.
- `evidence_blob`: ≤ 64 KiB.
- `confidence`: in `[0.0, 1.0]`.

### 3.3 Relation ops (0x0150–0x0156)

- `relation_type`: a `"namespace:name"` qname. Open-vocabulary in schemaless mode (interned on first use with `RelationTypeOrigin::ImplicitFromWrite`, default `cardinality: many_to_many`); strict mode rejects unknown qnames with `RelationTypeNotInSchema` (0x004C).
- `from`, `to`: must be existing entities; for schema-declared types, endpoint entity types must match the relation's declared signature → `ENTITY_TYPE_MISMATCH`. Implicit types skip this check.
- cardinality (`one_to_one` / `one_to_many` / etc.): enforced server-side on schema-declared types only → `CardinalityViolation` (0x0065).

### 3.4 Query ops (0x0160–0x0163)

- `top_k`: 1 ≤ top_k ≤ 1000.
- `depth` (for `RELATION_TRAVERSE`-shaped queries): 1 ≤ depth ≤ 8.
- `budget_wall_time_ms`: 1 ≤ budget ≤ 60000 (60s ceiling).
- empty filter clauses are allowed (no-op); empty `text` for `RECALL_HYBRID` rejected.

### 3.5 Admin ops (0x0170–0x0177)

- `audit_id`, `job_id`: non-zero UUIDv7; existence checked at handler.
- `extractor_ids` (for `ADMIN_BACKFILL`): non-empty; all must be registered.
- `memory_range`: `start ≤ end` (unix nanos) → `INVALID_ARGUMENT`.

## 4. Order of validation

Per request, the server runs validations in this order. The first failure short-circuits with the corresponding error:

1. **rkyv structural check** (`check_archived_root`). `MalformedRkyv` → close frame, keep connection.
2. **Universal field caps** (§1). `INVALID_ARGUMENT`.
3. **Op-specific field-level rules** (§2 / §3). Various codes per table.
4. **Cross-field rules** (e.g. `survivor != merged`). Op-specific codes.
5. **Existence / registry checks** (entity exists, type registered, schema declared). Op-specific codes.
6. **Idempotency replay check**. Cached response returned if `request_id` is a duplicate match; mismatch raises `IdempotencyConflict`.
7. **Handler proceeds.** Errors from this point are storage / commit failures (`Internal`).

## 5. Constants

Centralized so SDK clients and the server agree on the same numbers. Defined in code at `brain-core::knowledge::validation` (introduced in phase 17 along with statement-level rules):

```rust
pub const MAX_IDENT_BYTES: usize        = 256;
pub const MAX_FREEFORM_BYTES: usize     = 4096;
pub const MAX_BLOB_BYTES: usize         = 64 * 1024;
pub const MAX_COLLECTION_ELEMENTS: usize = 256;
pub const MAX_CURSOR_BYTES: usize       = 1024;
pub const MAX_ALIASES_PER_ENTITY: usize = 32;
pub const MAX_SCHEMA_DOCUMENT_BYTES: usize = 1024 * 1024;
pub const MAX_TOP_K: u32                = 1000;
pub const MAX_TRAVERSE_DEPTH: u32       = 8;
pub const MAX_QUERY_WALL_TIME_MS: u32   = 60_000;
pub const MIN_MERGE_CONFIDENCE: f32     = 0.7;
```

A future sub-task (tracked in [`./09_open_questions.md`](./09_open_questions.md)) makes these schema-overridable so operators can tighten them per-deployment.

## 6. Validation in tests

Each knowledge wire test (e.g. `crates/brain-server/tests/knowledge_entity_wire.rs`) covers at least:

- One success path per opcode.
- One negative path per **handler-level** rule (missing entity, unknown type, duplicate name, etc.).
- One negative path per **universal field cap** that is plausibly client-relevant (oversized name, too many aliases).

Rkyv-structural negatives (corrupted bytes) live in protocol-level tests under `crates/brain-protocol/`, not in the knowledge wire tests — they're substrate-layer concerns.

## 7. Open questions

See [`./09_open_questions.md`](./09_open_questions.md). Notably:

- Should aliases dedup also collapse common Unicode confusables (e.g. NFKC normalization)? Currently lowercase + whitespace-collapse only.
- Whether `attributes_blob` validation should run **inside** the wire layer (schema-aware) or stay opaque and be checked at the handler / planner. Currently opaque.
