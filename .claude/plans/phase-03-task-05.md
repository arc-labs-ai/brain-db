# Phase 3 — Task 3.5: Idempotency table with TTL

**Classification:** simple. One table, one value type, plus a pruning helper. No cross-table consistency; no symmetric mirroring. Uses the byte-array-key + rkyv-value pattern set by 3.2–3.4, with the only new element being the explicit TTL sweep.

**Spec:** `spec/07_metadata_graph/06_idempotency.md` (full — table shape, lookup-then-act protocol, conflict detection via canonical-form hash, 24h TTL, replay-not-re-execute, scope of idempotency-required ops). Cross-checked `spec/02_data_model/03_identifiers.md` §RequestId for the key shape.

## 1. Scope

In:

- `crates/brain-metadata/src/tables/idempotency.rs` (new):
  - `IDEMPOTENCY_TABLE: TableDefinition<'static, [u8; 16], IdempotencyEntry>` — key is `RequestId::to_be_bytes()`.
  - `IdempotencyEntry` (rkyv-derived) — see §3 for fields.
  - `pub mod response_kind` — u8 constants for `ENCODE`, `FORGET`, `LINK`, `UNLINK`, `UPDATE_KIND`, `UPDATE_CONTEXT`, `TXN_BEGIN`, `TXN_COMMIT` per spec §17.
  - `pub const DEFAULT_TTL_NANOS: u64 = 24 * 60 * 60 * 1_000_000_000;` — 24h per spec §6.
  - `pub fn prune_expired(table: &mut Table<'_, [u8; 16], IdempotencyEntry>, now_unix_nanos: u64, ttl_nanos: u64) -> Result<u64, redb::StorageError>` — deletes entries with `created_at + ttl_nanos < now`; returns count.
- `crates/brain-metadata/src/tables/mod.rs` — add `pub mod idempotency;`.

Out (deferred to later sub-tasks / phases):

- **Lookup-then-act handler logic** (spec §3): pure request-routing concern. Phase 9 (writer task) composes `idempotency.get()` then the mutation. 3.5 owns the table only.
- **Request canonicalization + hash computation** (spec §5). The storage layer stores a 32-byte `request_hash` and trusts the handler to fill it; canonicalization itself is Phase 9.
- **Background pruning scheduler** (spec §6: "every hour"). Phase 8 worker calls `prune_expired` on a cadence. 3.5 ships the pure function only.
- **`IdempotencyConflict` error variant on the wire protocol** (spec §5). Phase 3.10 / Phase 9 — error taxonomy lives in `brain-core`/`brain-protocol`.
- **`MetadataDb` wrapper methods** (`lookup_request`, `record_response`). Phase 3.10 — those compose the read txn, table open, get/insert.
- **Cross-shard routing assumptions** (spec §12). Phase 4.

## 2. Spec quotes that bind the design

> **§2 (the table):**
> ```rust
> table: idempotency
> key: RequestId
> value: IdempotencyEntry
> ```
> ```rust
> struct IdempotencyEntry {
>     response_kind: u8,
>     memory_id: Option<MemoryId>,
>     response_payload: Vec<u8>,
>     created_at: u64,
> }
> ```
>
> **§5 (conflict detection):** "The 'match' check uses a hash of the canonical form of the request. If the original hash doesn't match the new hash, the substrate returns `IdempotencyConflict`."
>
> **§6 (TTL):** "Default TTL is 24 hours, configurable. Entries older than the TTL are pruned by a background worker."
>
> **§9 (replay vs re-execute):** the response is stored verbatim and returned as-is. The storage layer just keeps the bytes.
>
> **§17 (scope):** the idempotency-required op list pins the `response_kind` enumeration to ENCODE, FORGET, LINK, UNLINK, UPDATE_KIND, UPDATE_CONTEXT, TXN_BEGIN, TXN_COMMIT. Each gets a stable u8.

## 3. Design decisions

### 3.1 Value layout — add `request_hash` to the v1 struct

Spec §2 literally lists four fields (`response_kind, memory_id, response_payload, created_at`). Spec §5 then says conflict detection needs a "hash of the canonical form of the request" — but doesn't say where it lives. Two options:

| Option | Notes |
|---|---|
| A. Recompute hash from `response_payload` on retry | Requires reversing the canonical form from the response. Spec §9 says response and request can differ (response includes server-generated `MemoryId`). Not derivable. |
| B. Store `request_hash: [u8; 32]` alongside | One extra 32 bytes per row. Conflict check is O(1) byte compare. |

**Picking B.** Spec §5 mandates the check be cheap and exact. The handler computes BLAKE3 over the canonical request bytes before calling `idempotency.get()`; on a hit it compares hashes. 32 bytes per row is negligible against the 50-byte/row figure spec §7 uses for capacity planning (which is itself dominated by `response_payload`).

This is **SD-3.5-1**: small deviation from spec §2's struct list, logged in `docs/spec-deviations.md`. The four spec-listed fields stay; we add one. Forward-compatible because rkyv struct-field additions are versioned via the `::v1` type_name.

### 3.2 `memory_id: Option<MemoryId>` encoded as `Option<[u8; 16]>`

Same pattern as 3.2/3.3/3.4 — byte representation in the rkyv struct, typed getter (`fn memory_id(&self) -> Option<MemoryId>`) at the API boundary. Brain-core's `MemoryId` doesn't derive rkyv (orphan rule), and we don't want a third crate-internal newtype.

### 3.3 `response_payload: Vec<u8>` — store verbatim, no validation

The handler hands us encoded response bytes. We store them and hand them back unchanged on replay. Spec §9 ("replays the original response verbatim") rules out any deserialize-then-reserialize step here.

### 3.4 `response_kind: u8` constants in a sub-module

Mirrors the pattern from 3.4's `origin` / `derived_by` modules. Values are stable wire identifiers — once shipped, do not renumber. The eight values per spec §17 get u8 `1..=8`; 0 reserved for "unknown / future use" so a stale reader can flag it.

```
response_kind::ENCODE         = 1
response_kind::FORGET         = 2
response_kind::LINK           = 3
response_kind::UNLINK         = 4
response_kind::UPDATE_KIND    = 5
response_kind::UPDATE_CONTEXT = 6
response_kind::TXN_BEGIN      = 7
response_kind::TXN_COMMIT     = 8
```

Promotion to brain-core deferred to the follow-up bundle alongside `EdgeKind`/`EdgeOrigin`/`MemoryKind` (now 4th deferred u8-mapping; promotion task gets bumped from "follow-up" to "tracked").

### 3.5 `prune_expired` — pure function over a table handle

Signature: `fn prune_expired(table: &mut Table<'_, [u8; 16], IdempotencyEntry>, now_unix_nanos: u64, ttl_nanos: u64) -> Result<u64, redb::StorageError>`.

Implementation: full iter + collect keys whose `created_at + ttl_nanos < now`, then remove. **Why collect first instead of streaming-delete:** redb's `iter()` borrows the table; calling `remove()` while iterating would conflict. Same approach the spec implicitly assumes ("delete them in batches"). For 86M rows/day (spec §7's worst-case figure), this scans the whole table per call — the Phase 8 worker calls it hourly, amortizing cost. v1 doesn't optimize further; spec doesn't pin a strategy beyond "incremental."

`now` and `ttl_nanos` are explicit parameters (no `SystemTime::now()` call inside). Keeps the function testable and decision-free at the storage layer.

### 3.6 Saturating arithmetic on `created_at + ttl_nanos`

`u64::MAX - u64::MAX` overflow only kicks in if both values approach `u64::MAX` (year 2554 + 24h). Use `saturating_add` for correctness without complexity.

### 3.7 Alignment fix from 3.4 applies pre-emptively

`AlignedVec` copy in `from_bytes`, same pattern as `MemoryMetadata` / `AgentMetadata` / `ContextMetadata` / `EdgeData`. No regression risk because every value type now uses the same template.

## 4. Files touched

- `crates/brain-metadata/src/tables/idempotency.rs` (new) — ~250 LOC including tests.
- `crates/brain-metadata/src/tables/mod.rs` — one new `pub mod idempotency;`.
- `docs/spec-deviations.md` — append **SD-3.5-1** (request_hash field added to IdempotencyEntry).
- `docs/phases/phase-03-metadata.md` — flip 3.5 to ✅ with "What was built" / "Done when" summary, post-implementation.

No edits to brain-core (deferred u8 promotion bundle).

## 5. Tests (gated `#[cfg(all(test, not(miri)))]`)

1. **`insert_and_get_round_trips`** — insert one entry, read it back, structural equality.
2. **`missing_key_returns_none`** — `.get(&rid).unwrap().is_none()`.
3. **`update_overwrites`** — second insert at same RequestId replaces the row.
4. **`memory_id_optional_round_trip`** — entry with `memory_id = None` and entry with `Some(...)` both round-trip.
5. **`response_payload_round_trip`** — non-trivial payload (256 bytes of varied content) round-trips byte-for-byte.
6. **`request_hash_byte_compare`** — two entries with different `request_hash` are distinct under `==`; same hash compares equal.
7. **`prune_expired_removes_old`** — insert one entry with `created_at = T0`, call `prune_expired(now=T0+25h, ttl=24h)`, assert removed and count is 1.
8. **`prune_expired_keeps_fresh`** — entry with `created_at = T0`, call `prune_expired(now=T0+1h, ttl=24h)`, assert still present and count is 0.
9. **`prune_expired_mixed`** — 3 old + 2 fresh entries; one call removes the 3 old, leaves the 2 fresh, returns 3.
10. **`prune_saturating`** — entry with `created_at = u64::MAX`, `prune_expired(now=0, ttl=86400_000_000_000)` does not panic (saturating add), keeps the entry.
11. **`encoding_stability_known_bytes`** — serialize a fixed `IdempotencyEntry`, hex-check the first 8 bytes match a recorded value. Same guard 3.2 set against accidental field reordering.

## 6. Verification

```
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p brain-metadata
just verify
```

Spec-lint skill not relevant (no spec edits). Brain-core unchanged.

## 7. Commit

Branch: `feature/brain-metadata` (continuing). One commit per AUTONOMY.md §5:

```
feat(brain-metadata): idempotency table with TTL pruning (sub-task 3.5)
```

Body summarizes: table shape, `IdempotencyEntry` fields, `request_hash` deviation (SD-3.5-1), `prune_expired` helper, 11 new tests. 8 of 13 tables done after this lands.

## 8. Done when

- [ ] `IDEMPOTENCY_TABLE` defined, opens cleanly in a fresh redb.
- [ ] Insert / get / delete / overwrite round-trip tested.
- [ ] `prune_expired` removes entries strictly older than the TTL and returns an accurate count.
- [ ] 11 tests green; `just verify` green.
- [ ] `docs/spec-deviations.md` records SD-3.5-1.
- [ ] `docs/phases/phase-03-metadata.md` 3.5 flipped to ✅.

PLAN READY.
