# Phase 16 · Sub-task 16.7 — Entity merge / unmerge / resolve / list / tombstone wire ops

Closes out the entity slice of the knowledge namespace. Five wire opcodes land end-to-end + entity event emission gets wired across all entity handlers (16.6c's `CREATE` / `GET` / `UPDATE` / `RENAME` get retro-fitted to emit too, since the emission path doesn't exist yet).

## Spec references (all already detailed enough)

- [`spec/28_knowledge_wire_protocol/01_entity_frames.md`](../../spec/28_knowledge_wire_protocol/01_entity_frames.md) §7–§11 — wire shapes for the five opcodes.
- [`spec/28_knowledge_wire_protocol/02_subscribe_events.md`](../../spec/28_knowledge_wire_protocol/02_subscribe_events.md) §3, §6 — `KnowledgeEventPayload`, emission semantics.
- [`spec/28_knowledge_wire_protocol/03_errors.md`](../../spec/28_knowledge_wire_protocol/03_errors.md) — error code mapping.
- [`spec/28_knowledge_wire_protocol/04_validation.md`](../../spec/28_knowledge_wire_protocol/04_validation.md) §2.5–§2.9 — field caps + per-op rules.
- [`spec/18_entities/02_storage.md`](../../spec/18_entities/02_storage.md) §"entity_merge_log" — extended `MergeRecord` shape.
- [`spec/18_entities/03_merge.md`](../../spec/18_entities/03_merge.md) §0–§11 — merge mechanics + phase-16.7 scope note.
- [`spec/18_entities/04_unmerge.md`](../../spec/18_entities/04_unmerge.md) — unmerge mechanics.
- [`spec/18_entities/05_garbage_collection.md`](../../spec/18_entities/05_garbage_collection.md) §2 — tombstone mechanics (16.6c already implemented `entity_tombstone` internally — 16.7 only adds the wire opcode).
- [`spec/03_wire_protocol/08_response_frames.md`](../../spec/03_wire_protocol/08_response_frames.md) §7 — extended `SubscriptionEvent`.

## Pre-flight spec updates (just landed, before this plan)

1. `§18/02 storage` — extended `MergeRecord` schema with reason / actor / aliases_added / attribute_conflicts / re-route counts / unmerge fields.
2. `§18/03 merge` §0 — phase-scope note: statement/relation re-routing deferred to 17/18.
3. `§18/03 merge` §4 — clarified operator-initiated vs system-initiated bands.
4. `§18/06 open_questions` — added Q10 (deferred re-routing sweep), Q11 (concurrent merge race), Q12 (re-merge after grace).
5. `§03/08 response_frames` §7 — extended substrate `SubscriptionEvent` with `knowledge_payload` and added knowledge event types.

## Sub-tasks

### 16.7.1 — Extend `MergeRecord` rkyv shape

**Reads:** §18/02 (new shape).
**Writes:** `crates/brain-metadata/src/tables/knowledge/merge.rs`.

- Replace existing 8-field `MergeRecord` with the spec's full shape: reason, actor_kind/actor_agent_bytes, aliases_added, trigrams_added, attribute_conflicts, statements_rerouted (always 0 in 16.7), relations_rerouted (always 0), mention_count_added, finalized, unmerged_at_unix_nanos, unmerged_by_actor_kind, unmerged_by_agent_bytes.
- New `AttributeConflictRecord` rkyv struct.
- Declare `ENTITY_MERGE_AUDIT_OVERFLOW` table per spec (empty in 16.7).
- Bump `impl_redb_rkyv_value!` version string `"::v1"` → `"::v2"`.
- Unit test: full round-trip with all fields populated.

**Done when:** redb roundtrip passes; existing tests still pass.

### 16.7.2 — `brain-metadata::entity_merge_ops` module

**Reads:** §18/03 §5 + §18/04 §4.
**Writes:** `crates/brain-metadata/src/entity_merge_ops.rs`.

Free functions over `WriteTransaction`:

```rust
pub fn merge_entity(
    wtxn: &WriteTransaction,
    survivor: EntityId,
    merged: EntityId,
    confidence: f32,
    reason: String,
    actor: MergeActor,
    grace_seconds: u64,        // default 7 days; configurable for tests
    now_unix_nanos: u64,
) -> Result<MergeId, EntityMergeOpError>;

pub fn unmerge_entity(
    wtxn: &WriteTransaction,
    merged_entity_id: EntityId,
    actor: MergeActor,
    now_unix_nanos: u64,
) -> Result<EntityId, EntityMergeOpError>;

pub enum MergeActor { System, Agent([u8; 16]) }

pub enum EntityMergeOpError {
    Storage(redb::StorageError),
    Table(redb::TableError),
    TrigramOp(TrigramOpError),
    EntityOp(EntityOpError),
    EntityNotFound(EntityId),
    SelfMerge,
    AlreadyMerged(EntityId),         // either side
    TypeMismatch { survivor: EntityTypeId, merged: EntityTypeId },
    Tombstoned(EntityId),
    LowConfidence(f32),
    OutOfGracePeriod,
    NotMerged(EntityId),             // unmerge of non-merged entity
    AuditMissing(MergeId),           // shouldn't happen
}
```

**Merge mechanics (§18/03 §5 phase-scoped):**

1. Load survivor + merged rows.
2. Pre-conditions per §18/03 §3.
3. Compute aliases_added (merged.aliases ∪ {merged.canonical_name} minus already-present in survivor); attribute_conflicts (per §6 default `survivor_wins`).
4. Mutate survivor in-memory: extend aliases, fold attributes, bump mention_count.
5. Set merged.merged_into = Some(survivor.id); merged.flags |= MERGED; merged.updated_at = now; merged.aliases = vec![] (parallel to tombstone path).
6. **Skip §5 steps 8-9** — statement/relation re-routing (phase 17/18).
7. Tear down merged's secondary indexes (canonical_name, aliases, trigrams).
8. Re-index survivor's new aliases and trigrams.
9. Write both rows back to ENTITIES_TABLE.
10. Allocate MergeId (UUIDv7); compute trigrams_added from new aliases + merged.canonical_name.
11. Write MergeRecord into MERGE_LOG_TABLE keyed by `(now_unix_nanos, merge_id_bytes)`.
12. Return MergeId.

**Unmerge mechanics (§18/04 §4 phase-scoped):**

1. Load merged entity. Find the MergeRecord by scanning MERGE_LOG_TABLE range filtered by merged_bytes. (Optimization: a `merged_by_merged_entity` secondary index — punt to phase 16.8 if simple scan is too slow; in v1.0 a per-entity O(log N) range scan is fine.)
2. Pre-conditions per §18/04 §3.
3. Restore merged.merged_into = None; merged.flags &= !MERGED; merged.updated_at = now.
4. Strip survivor: remove aliases_added; restore attributes per attribute_conflicts; subtract mention_count_added.
5. Skip statement/relation re-route restore (phase 17/18).
6. Re-add merged to secondary indexes (canonical_name, aliases, trigrams).
7. Strip survivor's secondary indexes of the aliases / trigrams that came only from merged.
8. Write both rows back; write audit's unmerged_at / unmerged_by fields.
9. Return survivor's EntityId.

Tests (host-runnable in `#[cfg(all(test, not(miri)))]`):

- Happy path: create A and B → merge(A as survivor, B) → verify B is redirected, indexes torn down, audit written.
- Unmerge round-trip: merge → unmerge → state matches pre-merge.
- All §3 pre-conditions (self-merge, type mismatch, double-merge, tombstoned, low confidence).
- Attribute conflict: B has attribute survivor lacks → survivor gains it; both have it → survivor wins.
- Grace boundary: unmerge succeeds at `expires - 1ns`, fails at `expires + 1ns`.
- Re-merge after unmerge.
- Re-merge after grace expires → rejected.

**Done when:** tests pass; `cargo zigbuild --target x86_64-unknown-linux-gnu -p brain-metadata --tests` clean.

### 16.7.3 — Wire structs in `brain-protocol::knowledge`

**Reads:** §28/01 §7–§11.
**Writes:** `crates/brain-protocol/src/knowledge/entity_req.rs`, `entity_resp.rs`, `mod.rs`.

Add request structs:
- `EntityMergeRequest` (§28/01 §7.1)
- `EntityUnmergeRequest` (§28/01 §8.1)
- `EntityResolveRequest` (§28/01 §9.1)
- `EntityListRequest` (§28/01 §10.1)
- `EntityTombstoneRequest` (§28/01 §11.1)

Add response structs:
- `EntityMergeResponse` (§28/01 §7.2)
- `EntityUnmergeResponse` (§28/01 §8.2)
- `EntityResolveResponse` + `ResolutionOutcome` u8 enum (§28/01 §9.2)
- `EntityListItem` + `EntityListResponseTail` (§28/01 §10.2)
- `EntityTombstoneResponse` (§28/01 §11.2)

Add `Opcode` variants:
- `EntityMergeReq = 0x0134`, `EntityMergeResp = 0x01B4`
- `EntityUnmergeReq = 0x0135`, `EntityUnmergeResp = 0x01B5`
- `EntityResolveReq = 0x0136`, `EntityResolveResp = 0x01B6`
- `EntityListReq = 0x0137`, `EntityListResp = 0x01B7`
- `EntityTombstoneReq = 0x0138`, `EntityTombstoneResp = 0x01B8`

Add `RequestBody` / `ResponseBody` variants + encode/decode/opcode-mapping arms.

Round-trip tests: one per new request, one per new response, opcode-byte assertions.

### 16.7.4 — Extend substrate `SubscriptionEvent` + knowledge events

**Reads:** §03/08 §7, §28/02 §3.
**Writes:**
- `crates/brain-protocol/src/responses/subscribe.rs` — extend `SubscriptionEvent` struct, `EventType` enum, add `KnowledgeEventPayload` union + all 14 event structs (§28/02 §3.1–§3.5).
- `crates/brain-ops/src/ops/subscribe.rs` — extend `EventEnvelope` with optional `knowledge_payload`; extend `to_wire()` mapping.

Round-trip tests for each new event variant.

Phase scope: only `KnowledgeEventPayload::EntityCreated / EntityUpdated / EntityRenamed / EntityMerged / EntityUnmerged / EntityTombstoned` are actually constructed in 16.7's emission paths. The Statement/Relation/Extraction/Schema variants are defined but never emitted yet — wire shape is forward-compat.

### 16.7.5 — Handlers for MERGE / UNMERGE / TOMBSTONE / LIST

**Reads:** §28/01 §7, §8, §10, §11 + §28/04 §2.5–§2.6, §2.8–§2.9.
**Writes:** `crates/brain-ops/src/ops/knowledge_entity.rs` (extend) + `crates/brain-ops/src/ops/knowledge_entity_list.rs` (new, for streaming).

Handler functions:

```rust
pub async fn handle_entity_merge(req: EntityMergeRequest, ctx: &OpsContext)
    -> Result<EntityMergeResponse, OpError>;
pub async fn handle_entity_unmerge(req: EntityUnmergeRequest, ctx: &OpsContext)
    -> Result<EntityUnmergeResponse, OpError>;
pub async fn handle_entity_tombstone(req: EntityTombstoneRequest, ctx: &OpsContext)
    -> Result<EntityTombstoneResponse, OpError>;
pub async fn handle_entity_list(req: EntityListRequest, ctx: &OpsContext)
    -> Result<EntityListResponse, OpError>;   // §16.7.6 makes this streaming
```

Each follows the 16.6c pattern: lock metadata, open redb txn, call into entity_ops / entity_merge_ops, commit, emit event, map errors.

### 16.7.6 — Streaming dispatch for `ENTITY_LIST`

**Reads:** §28/01 §10.2, §03/09 streaming.
**Writes:** `crates/brain-server/src/network/dispatch.rs` (extend OpDispatch path), `crates/brain-ops/src/ops/knowledge_entity_list.rs`.

The substrate's existing streaming model: per-frame, EOS on tail. Plumbed for `RECALL_RESP` / `PLAN_RESP` / `REASON_RESP` already. `ENTITY_LIST_RESP` follows the same pattern:

- Per-item frame body: `EntityListItem`.
- Tail frame body: `EntityListResponseTail` with `next_cursor` + `total_returned`.

Cursor: opaque rkyv-encoded `LastSeenEntityId` for the simple linear scan over `ENTITIES_TABLE`. Phase 16.9 may add index-aware cursor shapes for prefix-filtered scans.

Phase 16.7 ships single-shard list only — multi-shard fan-out is a phase-23 query-router concern.

### 16.7.7 — `ENTITY_RESOLVE` handler + resolver backend impls

**Reads:** §28/01 §9, §18/01.
**Writes:**
- `crates/brain-ops/src/ops/knowledge_entity_resolve.rs` — new file with the handler + backend impls.

Implement the resolver's three traits against the shard's runtime state:

```rust
// Local to this module; ResolverConfig overrides via OpsContext when needed.
struct ResolverStorageBackend<'a> { db_guard: parking_lot::MutexGuard<'a, MetadataDb> }
struct ResolverEmbedderBackend<'a> { embedder: &'a EmbeddingService }
struct ResolverIndexBackend<'a> { index: &'a EntityHnswIndex }

impl ResolverStorage for ResolverStorageBackend<'_> { ... }   // wraps entity_ops::lookup_by_canonical_name / by_alias / candidates_for_query
impl ResolverEmbedder for ResolverEmbedderBackend<'_> { ... }  // wraps brain_embed
impl ResolverIndex for ResolverIndexBackend<'_> { ... }        // wraps brain_index::EntityHnswIndex
```

Handler:

```rust
pub async fn handle_entity_resolve(req: EntityResolveRequest, ctx: &OpsContext)
    -> Result<EntityResolveResponse, OpError>;
```

1. Validate per §28/04 §2.7.
2. Acquire metadata lock, embedder handle (`ctx.executor.embedder`), entity HNSW handle (`ctx.executor.entity_hnsw` — needs adding to `ExecutorContext` if not there).
3. Call `brain_core::knowledge::resolve_entity(&storage, &embedder, &index, &req.candidate_name, &req.context, type_hint, &resolver_config)`.
4. Map `ResolutionOutcome` (brain-core) → wire `ResolutionOutcome` + tier + confidence + candidates / audit_id.
5. If outcome is `Created` and `req.allow_create = false`, instead write an ambiguity audit and return `NotFound` outcome.

**Pitfall:** if `EntityHnswIndex` isn't already in `ExecutorContext`, 16.7 adds it. Check that during implementation.

### 16.7.8 — Event emission across all entity handlers

**Reads:** §28/02 §3, §6.
**Writes:** edit every handler in `crates/brain-ops/src/ops/knowledge_entity.rs` + the new `knowledge_entity_resolve.rs` to call `ctx.events.publish(...)` after `wtxn.commit()`.

| Handler | Event emitted post-commit |
|---|---|
| `handle_entity_create` | `EntityCreatedEvent { entity_id, entity_type_id, canonical_name }` |
| `handle_entity_update` | `EntityUpdatedEvent { entity_id, entity_type_id, canonical_name, embedding_version_changed }` |
| `handle_entity_rename` | `EntityRenamedEvent { entity_id, old_canonical_name, new_canonical_name, old_moved_to_alias }` |
| `handle_entity_merge` | `EntityMergedEvent { survivor, merged, audit_id, confidence, statements_rerouted=0, relations_rerouted=0 }` |
| `handle_entity_unmerge` | `EntityUnmergedEvent { restored_entity_id, from_survivor, audit_id }` |
| `handle_entity_tombstone` | `EntityTombstonedEvent { entity_id, reason }` |
| `handle_entity_resolve` (Created outcome) | `EntityCreatedEvent` |

Emit order: **after** redb commit, **before** returning the handler's response. Per §28/02 §4.1.

### 16.7.9 — Dispatch arms + integration tests

**Writes:**
- `crates/brain-ops/src/dispatch.rs` — 5 new arms.
- `crates/brain-server/tests/knowledge_entity_merge.rs` — merge / unmerge end-to-end.
- `crates/brain-server/tests/knowledge_entity_resolve.rs` — resolve end-to-end (may need EmbedderTestDouble).
- `crates/brain-server/tests/knowledge_entity_list.rs` — list streaming end-to-end.
- `crates/brain-server/tests/knowledge_entity_tombstone.rs` — tombstone end-to-end + subsequent ENTITY_GET still works (returns tombstoned row).
- Extend `crates/brain-server/tests/knowledge_entity_wire.rs` — verify 16.6c handlers now emit events (subscribe + assert event arrives).

## Out of scope (this sub-task)

- Cross-shard merge (single-shard only in 16.7; phase 21+ tackles multi-shard).
- Statement / relation re-routing (deferred to 17/18 sweep — Q10).
- LLM-tier resolver (tier 4 stubbed; phase 21).
- System-initiated merge review queue (§18/03 §4.2; phase 21).
- SDK helpers (`brain-sdk-rust` typed entity API) — that's 16.8.
- ENTITY_LIST multi-shard fan-out — phase 23.

## Risks

- **`MergeRecord` schema change.** rkyv `v1` → `v2`. Pre-v1.0 so no migration path needed; any existing dev databases must be wiped. Document in commit message + test the fresh-db case.
- **Substrate `SubscriptionEvent` extension.** Touches every place that constructs / decodes the type. Phase-16.6b-style propagation expected — ~20 file edits. Use `cargo zigbuild --tests` early and often.
- **Resolver backend traits' lifetime story.** The current `MetadataDb` lock + `EntityHnswIndex` borrow + `EmbeddingService` borrow must coexist inside one call. May need to clone Arc handles or restructure. Validate during implementation; fall back to "hold metadata lock for the entire resolve call" if simpler.
- **Streaming dispatch for `ENTITY_LIST`.** First time a knowledge opcode streams; pattern mirrors substrate but the dispatch arm structure may need refactor. Inspect `run_op_dispatch` carefully.

## Order of operations + suggested commits

1. **Spec updates** (already landed above).
2. **16.7.1**: extend `MergeRecord`. One commit: `feat(metadata): MergeRecord v2 — full diff payload for unmerge (16.7.1)`.
3. **16.7.2**: `entity_merge_ops` module + tests. Commit: `feat(metadata): 16.7.2 — entity_merge_ops with merge / unmerge`.
4. **16.7.3 + 16.7.4**: wire structs + SubscriptionEvent extension. Commit: `feat(protocol): 16.7.3-4 — entity merge/unmerge/resolve/list/tombstone wire shapes + knowledge SUBSCRIBE events`.
5. **16.7.5 + 16.7.7 + 16.7.8**: handlers + resolver backends + event emission. Commit: `feat(ops): 16.7.5/7/8 — entity merge/unmerge/resolve/list/tombstone handlers + event emission`.
6. **16.7.6**: streaming dispatch for ENTITY_LIST. Commit: `feat(server): 16.7.6 — streaming dispatch for ENTITY_LIST`.
7. **16.7.9**: integration tests. Commit: `test(server): 16.7.9 — entity merge/unmerge/resolve/list/tombstone integration tests`.

Six commits total. Each compiles and tests independently — if any breaks, easy to bisect.

## Verification gate

Before any commit beyond 16.7.1:

- `cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests` clean.
- `cargo test -p brain-protocol` clean (no glommio deps; runs on host).
- `cargo test -p brain-core` clean.

Before declaring 16.7 complete:

- All 16.7.x tests pass.
- Integration tests under `brain-server/tests/knowledge_entity_*` pass on Linux CI.
- Clippy `-D warnings` clean.
- No unwrap() outside tests; expect("invariant: …") where unreachable.
- Spec/code consistent — re-check §28/01 §7–§11 wire shapes match the implemented structs byte-for-byte.

## Notes

- This is the **last** entity sub-task that adds wire opcodes. 16.8 (SDK helpers) and 16.9 (integration tests + benchmarks) close out phase 16.
- Per memory: no Co-Authored-By Claude trailer in commits.
- Per memory: folder-per-concern under src/ — `knowledge_entity.rs` stays monolithic but the new `_list.rs` and `_resolve.rs` files split out (the merge handler stays in `_merge.rs` if it grows past ~500 lines; otherwise extends the existing file).
- Branch: `feature/phase-16-entity-layer` (current).
