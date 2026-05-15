# Phase 15 — Knowledge-Layer Storage Extensions

> Plan for the first knowledge-layer phase. Covers sub-tasks 15.1–15.5.
> Surface for approval before implementation begins (plan-first convention).

## Goal

Add the on-disk structures the knowledge layer needs — without disturbing any substrate behavior. After this phase:

- The binary boots against an existing substrate data directory.
- 24 new redb tables exist (empty) inside `metadata.redb`.
- The WAL frame-type discriminator accepts new kinds (write-noop today).
- New on-disk artifact paths (`statements.tantivy/`, `entity.hnsw`, `statement.hnsw`, `llm_cache.redb`) are created on `Shard::open()` if missing.
- Substrate-only acceptance + benchmarks continue to pass within 110% of pre-phase baselines.

**Activation model:** the knowledge layer is a **core feature**, not a togglable one. There is no `BRAIN_KNOWLEDGE_ENABLED` env var or config flag. The on-disk structures exist on every deployment; the knowledge layer is "active" iff a schema has been uploaded (the persistent schema-declared flag set by `SCHEMA_UPLOAD`). Substrate-only is a *consequence of not declaring a schema*, not a deployment switch.

**Non-goals (deferred to later phases):**

- No knowledge-layer behavior (entity/statement/relation CRUD lands phases 16–18).
- No tantivy or new HNSW crate dependency yet (phases 22 and 16).
- No wire-opcode handlers (phases 16+).

## Spec references (read in order)

1. [`spec/26_knowledge_storage/00_purpose.md`](../../spec/26_knowledge_storage/00_purpose.md) — the canonical layout (tables, indexes, frame types, sizing).
2. [`spec/17_knowledge_model/00_purpose.md`](../../spec/17_knowledge_model/00_purpose.md) — context for what each table holds (entities vs statements vs relations).
3. [`spec/05_storage_arena_wal/05_wal_records.md`](../../spec/05_storage_arena_wal/05_wal_records.md) — existing WAL record encoding (32 B record header + variant body); the new kinds reuse the same header format.
4. [`spec/07_metadata_graph/02_table_layout.md`](../../spec/07_metadata_graph/02_table_layout.md) — existing redb table catalog (13 tables); we add 25 alongside.
5. [`AUTONOMY.md`](../../AUTONOMY.md) §23 — knowledge-layer scope; "schema-optional behavior is binding."

## Pre-flight findings (anchor the plan)

### F-1 — `WalRecordKind` decimal/hex collision is in fact consistent

`crates/brain-storage/src/wal/kinds.rs` uses decimal 1–15 today and rejects ≥16 as "reserved for v1 minor." Spec §26's knowledge-layer frame types `0x10..0x50` are the same numeric range (`0x10 == 16`). They line up — no renumber needed. We update the rejected-range comment + the negative-test (`from_u8(16) == None` no longer holds after 15.2) when 15.2 lands.

### F-2 — Spec §26 says "25 redb tables"; the enumeration lists 24

Counting the bundle's table table: 24 rows. The "25" in the prose may be off-by-one or count the LLM cache file as one of the 25. I'll go by the enumeration (24 tables in `metadata.redb`, 2 more in `llm_cache.redb` per a separate sub-task).

### F-3 — `Shard::open()` path

Need to find `Shard::open()` in brain-storage (or wherever the on-disk artifact directory is initialized). Reconnaissance during 15.3.

### F-4 — Substrate-only regression test

Phase 15.5 wires `tests/knowledge_compat.rs` (renamed from bundle's `tests/v2_compat_v1.rs`). This must:

- Boot a server against an empty data directory (no schema uploaded → knowledge layer inert by definition).
- Run the substrate acceptance suite (or a representative subset — full §16 acceptance comes in Phase 14).
- Assert P50 / P99 ENCODE + RECALL latencies within 110% of a pre-phase baseline.

The baseline numbers come from Phase 13's criterion benches (`docs/performance/baselines-*.md`). 15.5 stores its own baseline and diffs.

The test is **not** asserting "knowledge layer disabled" — there's no such mode. It's asserting that adding the knowledge layer's on-disk structures + WAL kind discriminator does not regress substrate hot-path latency when no schema is declared.

## Sub-task plan (mapped to phase doc 15.1–15.5)

Each sub-task is one commit. Sub-tasks are independent enough to land in order without cross-dependence except 15.5 (regression test) which depends on everything else.

**Note on renumbering:** the bundle's phase doc had 6 sub-tasks; sub-task 15.5 ("knowledge-mode server config flag") has been **dropped** because the knowledge layer is a core feature with no enable/disable toggle. The original 15.6 (regression test) is now 15.5. Phase doc updated to match.

### 15.1 — Define 24 new redb tables in `brain-metadata`

**Reads:** spec §26 (table catalog).
**Writes:**
- `crates/brain-metadata/src/tables/knowledge/mod.rs` — module index.
- `crates/brain-metadata/src/tables/knowledge/{entity,statement,relation,predicate,entity_type,relation_type,extractor,schema_version,audit,merge}.rs` — one file per logical table family (10 files covering 24 tables).
- `crates/brain-metadata/src/lib.rs` — re-export.

**Approach:**
- Each table file declares `redb::TableDefinition<'_, K, V>` constants with the spec's key/value types.
- Values are stub structs with `#[derive(Archive, Deserialize, Serialize)]` (rkyv); for 15.1 they only need to compile — no real fields yet beyond the spec'd ones.
- `EntityId`, `StatementId`, `RelationId`, `PredicateId`, `ExtractorId`, etc. are new types in `brain-core` — define as `Uuid` wrappers (UUIDv7), `#[repr(transparent)]`, derive PartialEq/Eq/Hash/Ord/Copy/Clone/Debug.

**Done when:** `cargo check -p brain-metadata` is green; tables compile with correct key/value type signatures; nothing imports them yet outside the module.

**Pitfalls:** Don't import any knowledge-layer *behavior* yet — only types. Keep the module isolated so substrate code is unaffected. Don't add fields beyond what the spec mandates today; later phases tighten.

### 15.2 — Knowledge-layer WAL frame kind discriminator

**Reads:** spec §26 (WAL frame types section); `crates/brain-storage/src/wal/kinds.rs`; `wal/record.rs` (variant dispatch).
**Writes:**
- `crates/brain-storage/src/wal/kinds.rs` — add 13 new variants (per spec §26: `EntityCreate=16`, `EntityUpdate=17`, `EntityMerge=18`, `EntityTombstone=19`, `StatementCreate=32`, `StatementSupersede=33`, `StatementTombstone=34`, `RelationCreate=48`, `RelationSupersede=49`, `RelationTombstone=50`, `SchemaUpdate=64`, `Audit=80`).
- Update `from_u8` to accept these; update `ALL_KINDS`; update `from_u8_rejects_reserved_and_unknown` negative test (16 is now valid; pick a still-reserved value for the negative case — e.g., 96 / 128).
- `crates/brain-storage/src/wal/record.rs` — add `WalRecord::Knowledge(KnowledgePlaceholder)` variant whose body is the raw rkyv payload bytes (parse-as-bytes placeholder; phases 16–19 add real variant bodies).
- `crates/brain-storage/src/wal/reader.rs` — recognize new kinds; round-trip through reader as placeholder.

**Approach:**
- Frame **header is unchanged** (32 bytes); the kind byte selects parsing. Discriminator goes from u8 to u8 (same width).
- New kinds carry **no** structured body in this phase — recovery treats them as opaque. We only need: writer can produce them (used by tests), reader can step over them, replay is a noop for substrate state.
- CRC computation already includes the kind byte (it's part of the header). No CRC changes.

**Done when:** WAL writer accepts new frame kinds (placeholder bodies); reader recognizes + skips them; substrate frame parsing remains intact; round-trip test for every new kind.

**Pitfalls:** Don't increment WAL `format_version`. New kinds are additive within v1. If anything in `recovery.rs` exhaustively matches `WalRecordKind`, add no-op arms for the new kinds (don't unwrap-or-panic; just continue).

### 15.3 — On-disk artifact directory layout

**Reads:** spec §26 (per-shard layout); `crates/brain-storage/src/lib.rs` + wherever `Shard::open` lives.
**Writes:**
- `crates/brain-storage/src/layout.rs` (new file) — `KnowledgeArtifactPaths { entity_hnsw, statement_hnsw, statements_tantivy_dir, memory_text_tantivy_dir, llm_cache_db }`.
- Modify `Shard::open` (or its equivalent) to call `mkdir -p` on each directory and ensure parent dirs exist for the file artifacts. Don't *create* the files (HNSW + tantivy will write them when first used).
- Test: open a substrate-only data dir → assert all knowledge paths exist + are empty + don't disturb existing substrate files.

**Done when:** `Shard::open()` creates the new dirs/parents idempotently; substrate shards open without error; existing tests stay green.

**Pitfalls:** Use `create_dir_all` (idempotent). Don't fsync the directory creates — substrate startup is not the durable-write moment. Existing data dirs must still open after upgrade (tested explicitly).

### 15.4 — LLM cache redb file

**Reads:** spec §26 (LLM cache section).
**Writes:**
- `crates/brain-metadata/src/llm_cache.rs` (new) — `LlmCacheDb` struct opening a separate redb file with two tables:
  - `llm_responses: (input_hash: [u8; 32], extractor_id: ExtractorId, extractor_version: u32, model_id: u64) -> Vec<u8>` (rkyv-encoded raw response)
  - `llm_response_ttl: (expiry_unix_secs: u64, cache_key: [u8; 32]) -> ()`
- `crates/brain-storage/src/layout.rs` — wire the path: `data/shard-NNN/llm_cache.redb`.
- `Shard::open` opens both `metadata.redb` and `llm_cache.redb`.

**Approach:**
- Separate redb file because the cache may grow to GBs and shouldn't bloat the hot metadata file.
- 15.4 creates + opens the file; no read/write API beyond table init.

**Done when:** opening a shard creates both redb files; tables initialize; no entries inserted.

**Pitfalls:** Keep the cache file **separate** from `metadata.redb`. Don't share the redb instance — separate `Database::create`. Both must respect the same fsync discipline (redb's default is fine).

### 15.5 — Substrate-only regression test

**Reads:** spec §16 (acceptance); Phase 13's `docs/performance/baselines-*.md`.
**Writes:**
- `tests/knowledge_compat.rs` (workspace-level integration test) — boots a server against an empty data dir (no schema uploaded → knowledge layer inert), runs a representative substrate workload (N ENCODEs + M RECALLs + K FORGETs), captures P50/P99 timings, compares to a recorded baseline.
- `tests/fixtures/knowledge_compat_baseline.json` — committed baseline timings (small; updated when intentional perf changes ship).

**Approach:**
- Test runs single-shard, in-process. No need for TCP — call into the server library entry points directly.
- Threshold: P50 + P99 within 110% of baseline. P99.9 not asserted (too noisy in CI).
- Mark test `#[ignore]` if it's flaky on CI hardware; add a `just bench-compat` recipe that runs it explicitly. (Decision deferred until first run shows noise level.)

**Done when:**
- Test compiles, runs, and passes on Linux CI.
- Baselines file committed.
- P50/P99 latencies within 110% of Phase 13's baselines for ENCODE and RECALL.

**Pitfalls:** Run on substrate reference data only — don't accidentally pull in knowledge-layer code paths. Check **tail** latencies, not just averages — the spec target is P99 not mean.

## Phase exit checklist

- [ ] 15.1 — 24 new redb tables compile with correct key/value sigs.
- [ ] 15.2 — 13 new WAL kinds accepted; round-trip green; reserved range updated.
- [ ] 15.3 — `Shard::open()` creates new dirs idempotently on substrate + new dirs.
- [ ] 15.4 — `llm_cache.redb` opens with both tables initialized.
- [ ] 15.5 — `tests/knowledge_compat.rs` green; baselines committed.
- [ ] All substrate tests still pass; `just verify` green.
- [ ] Tag `phase-15-complete` after final commit.

## Risk register

| Risk | Mitigation |
|---|---|
| WAL kind variant exhaustive `match` blows up | Audit `recovery.rs` + `payload.rs` + `reader.rs` during 15.2; add explicit no-op arms. |
| redb table type erasure causes lifetime headaches | Existing tables in `tables/` already solve this — copy the pattern. |
| LLM cache file creates on substrate-only deployments wastes disk | The empty redb file is ~32 KB. Acceptable. |
| `Shard::open` test races on parallel test runs | Use `tempfile::tempdir()` per test; never share a fixed path. |
| 15.5 baseline is fragile on shared CI hardware | Mark `#[ignore]` and run via `just bench-compat` if we see >110% drift in CI noise; rerun on a quiet machine for the canonical numbers. |

## Crate dependency additions

- `brain-core`: no new crate deps; only type additions.
- `brain-metadata`: no new crate deps.
- `brain-storage`: no new crate deps.
- `brain-server`: no new crate deps.
- **No new dependencies in Phase 15.** Tantivy + LLM clients + extractor models all land later.

## Commit cadence

One commit per sub-task following the project format:

```
feat(metadata): 15.1 — add 24 knowledge-layer redb tables (empty)
feat(storage): 15.2 — knowledge-layer WAL frame kind discriminator
feat(storage): 15.3 — on-disk paths for knowledge-layer artifacts
feat(metadata): 15.4 — LLM cache redb file
test(knowledge): 15.5 — substrate-only regression suite + baselines
```

After 15.5 passes: `git tag phase-15-complete`.

## What I need from you before starting

Approval on:

1. **Scope** — anything in "Non-goals" you want pulled into Phase 15 instead?
2. **WAL kind numbering** — agree we use spec §26's hex range (16–80 decimal) and document the reserved-range update in 15.2?
3. **Regression test threshold** — 110% of Phase 13 baseline OK? Or stricter (105% would be tighter but more CI-noise-prone)?
4. **Plan-file shape** — one phase file like this, or split into 15.1.md … 15.5.md per-task plans like phase-10? I lean phase-file given the sub-tasks are small; happy to split if you want per-task ADRs.

On your nod I start with 15.1 and proceed sub-task → commit → next sub-task until 15.5.
