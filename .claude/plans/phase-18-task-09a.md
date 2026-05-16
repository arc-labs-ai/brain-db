# 18.9a — Relation integration tests + lifecycle

End-to-end coverage for the relation layer. Mirrors 17.10a's
structure exactly.

## Files

1. **`crates/brain-server/tests/knowledge_relation_wire.rs`** —
   Linux-only per-op wire smoke. Covers create / get / supersede /
   tombstone / list_from / list_to / traverse + error paths.
2. **`crates/brain-server/tests/knowledge_relations_phase_exit.rs`** —
   Linux-only full-lifecycle test: create symmetric + asymmetric
   relations, cardinality auto-supersede, list, traverse, tombstone,
   re-traverse.
3. **`crates/brain-sdk-rust/tests/knowledge_relation.rs`** — Mock-
   server SDK tests (runs on host).

## Built-in relation type extensions

Add to `BUILTIN_RELATION_TYPES` in `db.rs`:

- `brain:reports_to` — `ManyToOne`, asymmetric. For cardinality auto-
  supersede tests.
- `brain:co_authored` — `ManyToMany`, symmetric. For symmetric
  dual-index tests.

Reuses the existing `brain:related_to` for the generic case.

## Tests

### `knowledge_relation_wire.rs` (~11 tests)

- `create_asymmetric_round_trips` — create + get returns same view.
- `create_unknown_relation_type_returns_error` — `INVALID_ARGUMENT`.
- `create_unknown_endpoint_returns_error` — entity not found.
- `create_symmetric_canonicalises` — caller passes a, b with a > b;
  server stores canonical.
- `create_many_to_one_auto_supersedes` — second create with same
  `from` auto-supersedes; second response carries a new id; old
  is no longer current via list_from.
- `get_missing_returns_error`.
- `supersede_explicit_returns_new_id_and_version`.
- `tombstone_flips_current_state` — list_from with default
  current_only excludes; with include_tombstoned sees the row.
- `list_from_filters_by_type`.
- `list_to_filters_by_type`.
- `traverse_one_hop` / `traverse_two_hop` / `traverse_depth_invalid`.

### `knowledge_relations_phase_exit.rs` (1 lifecycle test)

Steps:
1. Create Person entities A, B, C.
2. Create asymmetric `brain:knows` A → B and B → C (use
   `brain:related_to` since `brain:knows` isn't a built-in).
3. Traverse from A depth 2 → 2 paths.
4. Create symmetric `brain:co_authored` A ↔ B; query list_from(B)
   sees it.
5. Tombstone A→B.
6. Re-traverse from A → 0 paths (current_only excludes tombstoned).

### `knowledge_relation.rs` SDK (~8 tests)

Per-builder mock-server tests:
- `relation_builder_create` — full chain.
- `relation_get_translates_not_found_to_none`.
- `relation_supersedes_routes_to_supersede_op`.
- `relations_tombstone_returns_timestamp`.
- `relations_list_from`.
- `relations_list_to`.
- `relations_traverse`.
- `relation_unknown_type_classifies_via_extension_trait`.

## Verify

```
cargo zigbuild --target x86_64-unknown-linux-gnu --workspace --tests
cargo test -p brain-sdk-rust knowledge_relation
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```

brain-server tests don't run on macOS (glommio); cross-compile is
the floor.

## Risks

- **Without SCHEMA_UPLOAD**, integration tests are limited to built-
  in relation types. Adding `brain:reports_to` + `brain:co_authored`
  to the seeded set unlocks cardinality + symmetric coverage.
- **Subscribe-event assertions** in the lifecycle test — same
  deferral as entity/statement (17.10a notes: drop for now, the
  entity-side phase-exit doesn't assert events).
- **`brain:knows` deliberately reused as `brain:related_to`** in
  tests — `related_to` is ManyToMany asymmetric per the seed, so
  the lifecycle test uses it directly.

## Commit message draft

```
test(brain-server,brain-sdk-rust): relation integration tests (18.9a)

Three new test files cover the relation layer end-to-end:
- knowledge_relation_wire.rs (brain-server): 11 wire-smoke tests.
- knowledge_relations_phase_exit.rs (brain-server): 6-step
  lifecycle (create + symmetric + cardinality + traverse +
  tombstone + re-traverse).
- knowledge_relation.rs (brain-sdk-rust): 8 mock-server SDK
  tests.

Built-in relation types extended in db.rs with brain:reports_to
(ManyToOne) and brain:co_authored (symmetric ManyToMany) so
integration tests cover cardinality + symmetric without a
SCHEMA_UPLOAD path (phase 19).

Linux-only via `#![cfg(target_os = "linux")]`. Cross-compile
verified on macOS via cargo zigbuild --target x86_64-unknown-linux-gnu.

Plan: .claude/plans/phase-18-task-09a.md.
```
