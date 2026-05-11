# Phase 3 — Task 3.8: `model_fingerprints` + `next_lsn`

> **Realignment note.** The phase doc originally titled 3.8 "Counters and statistics" with `Done when: Per-shard counters (memory count, edge count, etc.) reconcile from full scans.` That isn't a stored table — it's a derivation that walks `memories`/`edges_out`, and the denormalized fields it would feed (`AgentMetadata.memory_count`, `ContextMetadata.memory_count`) already exist on row types from 3.3. The spec catalog (`spec/07_metadata_graph/02_table_layout.md` §1) has two unaccounted-for tables in the remaining Phase 3 budget: `model_fingerprints` and `next_lsn`. This sub-task bundles both. Reconcile-from-scans logic, when needed, lands in `MetadataDb` (3.10) or the maintenance worker (Phase 8) — not as a storage primitive.
>
> The post-implementation phase-doc update records the realignment in-line, same pattern as 3.7.

**Classification:** simple. Two tables, both with low surface area. `model_fingerprints` carries one variable-length value type (`ModelInfo` with a `String` field, rkyv-derived in the established 3.x pattern); `next_lsn` is a singleton scalar using redb's `()` key.

**Spec:** `spec/04_embedding_layer/07_fingerprinting.md` §8 (full table shape + value fields); `spec/07_metadata_graph/02_table_layout.md` §1 row 10 + §1 row 12 (singleton). Cross-checked §07/02 §7 ("We use redb's `()` key type for singletons") and §04/07 §11 (`ADMIN_REGISTER_MODEL` is the registration path — composition for Phase 9, not 3.8).

## 1. Scope

In:

- `crates/brain-metadata/src/tables/model_fingerprint.rs` (new):
  - `MODEL_FINGERPRINTS_TABLE: TableDefinition<'static, [u8; 16], ModelInfo>` — keyed by `ModelFingerprint::to_bytes()` (raw 16-byte BLAKE3-truncate per spec §04/07 §2, §13).
  - `ModelInfo { model_name: String, seen_at_unix_nanos: u64, memory_count_at_fingerprint: u64 }` — rkyv-derived (`Archive, Serialize, Deserialize`, `check_bytes`, `::v1` type_name) following the 3.2/3.3/3.4/3.5 pattern.
  - `ModelInfo::new(model_name, seen_at_unix_nanos)` constructor; `memory_count_at_fingerprint: 0` initially.
  - `redb::Value` impl with the `AlignedVec` workaround (consistent with every prior rkyv value type).
- `crates/brain-metadata/src/tables/next_lsn.rs` (new):
  - `NEXT_LSN_TABLE: TableDefinition<'static, (), u64>` — singleton.
  - No helper functions. `t.get(&())` / `t.insert(&(), &v)` is what spec §07/02 §7 prescribes; wrapping it would add noise without value.
- `crates/brain-metadata/src/tables/mod.rs` — add `pub mod model_fingerprint;` and `pub mod next_lsn;`.

Out:

- **Auto-registration on first-seen fingerprint** (spec §04/07 §5). Phase 4 / wire layer composes ENCODE → `model_fingerprints.insert(if absent)`. 3.8 owns the table only.
- **`ADMIN_REGISTER_MODEL` opcode** (spec §04/07 §11). Wire-layer + writer task (Phase 9).
- **`memory_count_at_fingerprint` maintenance** — denormalized; maintenance worker reconciles (Phase 8).
- **`UnknownModel` rejection on `ENCODE_VECTOR_DIRECT`** (spec §04/07 §11). Wire-layer.
- **LSN allocation logic** — `MetadataSink` impl (3.11) reads `next_lsn`, hands out, advances, persists. 3.8 ships the table only.
- **Initial-value-on-missing semantics for `next_lsn`** — spec doesn't pin (recovery seeds it from WAL scan; fresh shard starts at LSN 0 or 1 — Phase 9 decides). Storage stays decision-free.
- **Counters/statistics reconciliation** — see realignment note above.

## 2. Spec quotes that bind the design

> **§04/07 §8 (fingerprint table):**
> ```
> table: model_fingerprints
> key: ModelFingerprint
> value: {
>     model_name: String,
>     seen_at: u64,
>     memory_count_at_fingerprint: u64
> }
> ```
> Purpose: "Lets operators see the fingerprint history of a shard. Supports `ADMIN_STATS` queries about model migration progress. Helps diagnose 'why are my queries returning fewer results than expected?'."
>
> **§04/07 §2:** `ModelFingerprint = [u8; 16]`. → fixed-width key, byte-identical to brain-core's representation (assuming the type lands there).
>
> **§07/02 §1 row 12:** `next_lsn | () | u64 | The next WAL LSN (singleton)`.
>
> **§07/02 §7:** "We use redb's `()` key type for singletons. Reading is `table.get(&())`, writing is `table.insert(&(), &value)`."

## 3. Design decisions

### 3.1 `[u8; 16]` key for fingerprints — same pattern as `MemoryId`/`AgentId`

Brain-core may or may not already carry a `ModelFingerprint` newtype (`grep` in plan-phase is read-only; quick check during implementation will confirm). If it exists, we expose a typed getter; if not, we store and accept raw `[u8; 16]` and keep the typed wrapper as a follow-up. Either way the on-disk key is 16 raw bytes — same orphan-rule argument as 3.2/3.3 (brain-core types don't derive rkyv).

### 3.2 `ModelInfo` field names: `seen_at_unix_nanos`, `memory_count_at_fingerprint`

Spec field is named `seen_at`. We append `_unix_nanos` to match the time-field convention every other 3.x value type adopted (`created_at_unix_nanos`, `forgot_at_unix_nanos`, `last_active_at_unix_nanos`). Renaming the spec field at the storage layer is a tiny, consistent stylistic choice; the typed getter `seen_at_unix_nanos(&self) -> u64` is the public name. **Not** logged as SD — the spec doesn't pin a field-name convention.

`memory_count_at_fingerprint` is kept verbatim; it's distinctive enough that "memory_count" alone would be ambiguous with `AgentMetadata.memory_count`.

### 3.3 No helper functions

`MODEL_FINGERPRINTS_TABLE` and `NEXT_LSN_TABLE` are both stand-alone tables whose only operations (`insert`, `get`, possibly `remove`) are redb built-ins. 3.5's `prune_expired` and 3.7's `increment` exist because each does read-modify-write logic worth one named function; here there's no equivalent. Resist the urge to ship a `register_or_update` for fingerprints — that's Phase 9's auto-register flow, and the canonical "what does insert mean here" answer differs depending on whether we're auto-registering on encode or replaying an `ADMIN_REGISTER_MODEL`.

### 3.4 No `type_name` versioning for `next_lsn`

Same reasoning as 3.6/3.7: built-in scalar value (`u64`), nothing to evolve. redb's built-in `u64` `type_name` is fine.

### 3.5 `()` key type — confirm redb v4 supports it

Spec §07/02 §7 explicitly prescribes `()`. redb's standard library impls include `()` as `Key + Value` (zero-byte encoding). If somehow it doesn't compile, we'd fall back to `u8 = 0u8` as a one-byte sentinel and log SD-3.8-1 — but the spec wouldn't have prescribed `()` if redb didn't support it, and quick search confirms it does. Mentioning the fallback so we're not surprised if a v4 API change broke it.

### 3.6 Alignment-copy workaround preemptive in `ModelInfo`

Every rkyv-derived `Value` type uses the `AlignedVec` copy in `from_bytes` (added in 3.4 after `EdgeData` failed `Underaligned`). Apply preemptively to `ModelInfo`. No need to test the alignment behaviour separately — the round-trip tests cover it.

## 4. Files touched

- `crates/brain-metadata/src/tables/model_fingerprint.rs` (new) — ~150 LOC including tests.
- `crates/brain-metadata/src/tables/next_lsn.rs` (new) — ~60 LOC including tests.
- `crates/brain-metadata/src/tables/mod.rs` — two new `pub mod` lines (kept alphabetical: `model_fingerprint`, `next_lsn`).
- `docs/phases/phase-03-metadata.md` — flip 3.8 to ✅ with realignment recorded, post-implementation.

No edits to brain-core (no new types). No new SD entry (realignment is *to* the spec).

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

### model_fingerprint.rs (6 tests)

1. **`insert_and_get_round_trips`** — write one row, read back, structural equality.
2. **`model_info_with_long_name_round_trips`** — `String` field exercises rkyv's variable-length path (and the alignment-copy fix).
3. **`update_overwrites`** — second insert at same fingerprint replaces; `memory_count_at_fingerprint` reflects the bumped value.
4. **`missing_key_returns_none`** — vanilla redb-behaviour pin.
5. **`multiple_fingerprints_coexist`** — two distinct fingerprints round-trip independently.
6. **`type_name_includes_v1`** — `format!("{:?}", <ModelInfo as Value>::type_name())` contains "v1".

### next_lsn.rs (4 tests)

1. **`singleton_insert_and_get_round_trips`** — write `42u64`, read back via `t.get(&())`.
2. **`singleton_update_overwrites`** — second insert replaces.
3. **`singleton_missing_returns_none`** — fresh table, `t.get(&())` is `None`.
4. **`unit_key_round_trips`** — sanity: redb v4 still supports `()` as Key + Value. (This is what guards §3.5's fallback note.)

## 6. Verification

Same Linux dev-container harness:

```
docker run --rm -v "$(pwd)":/workspaces/brain ... brain-dev:latest \
  bash -c "cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p brain-metadata"
```

Expected: 73 brain-metadata tests pass (63 prior + 10 new).

## 7. Commit

Branch: `feature/brain-metadata` (continuing). AUTONOMY §5 format:

```
feat(brain-metadata): model_fingerprints + next_lsn tables (sub-task 3.8)
```

Body summarises: realignment, two tables, ModelInfo shape, no helpers (insert/get is everything), 10 new tests. **12 of 13 spec'd tables done.** Only `checkpoints` remains (3.9).

## 8. Done when

- [ ] `MODEL_FINGERPRINTS_TABLE` defined; opens cleanly; ModelInfo round-trips including long `String` names.
- [ ] `NEXT_LSN_TABLE` defined; singleton round-trips through `t.insert(&(), &v)` / `t.get(&())`.
- [ ] 10 tests green; full brain-metadata suite green in the container.
- [ ] `docs/phases/phase-03-metadata.md` 3.8 retitled to "`model_fingerprints` + `next_lsn` tables" (✅), with the realignment note (phase-doc "counters and statistics" had no spec backing; reconcile-from-scans deferred to 3.10/Phase 8).

PLAN READY.
