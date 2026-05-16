# 19.10a — Integration tests

End-to-end coverage for the phase-19 schema surface. Three test
files:

1. `knowledge_schema_wire.rs` — wire smoke for all 4 schema opcodes
   + error paths (parse error, validation error, empty / oversized
   document, missing namespace on GET).
2. `knowledge_schema_phase_exit.rs` — lifecycle: upload v1 → list →
   get → upload v2 → assert active is v2 → validate dry-run against
   would-be v3.
3. `crates/brain-sdk-rust/tests/knowledge_schema_sdk.rs` — SDK
   round-trip against a live in-process server.

The SDK test exists because the phase-19 spec calls out the
`client.schema()` ergonomics as part of the phase-exit deliverable
(§29/00).

## Files written

| Path | Purpose |
|---|---|
| `crates/brain-server/tests/knowledge_schema_wire.rs` | Wire smoke. |
| `crates/brain-server/tests/knowledge_schema_phase_exit.rs` | Lifecycle phase exit. |
| `crates/brain-sdk-rust/tests/knowledge_schema_sdk.rs` | SDK round-trip. |

The `support_harness` mount is the same boilerplate as
`knowledge_entity_wire.rs`. SDK test reuses brain-server's harness
via a `mod support_harness;` mount (path-local include).

## Wire-test cases

### `knowledge_schema_wire.rs`

1. **Upload + get + list smoke** — single-shard server; upload a
   simple namespace schema, GET active version (v0 = active=1),
   LIST returns one entry.
2. **Validate dry-run** — VALIDATE returns `would_be_version = 1`
   on empty active.
3. **Upload bumps version** — second upload returns v2; LIST
   newest-first reports [v2, v1].
4. **Parse error → validation_errors populated** — malformed input
   (`namespace 123`) → `schema_version == 0`, `validation_errors`
   non-empty, with `code = "Syntax"`.
5. **Validation error → validation_errors populated** — reserved
   `namespace brain` → `code = "NamespaceInvalidIdentifier"`.
6. **Empty document → ERROR frame** — empty `schema_document`
   yields an `Error` response with `InvalidRequest`.
7. **GET unknown namespace → ERROR with `NotFound`**.
8. **GET active when none uploaded → ERROR with `NotFound`**.

### `knowledge_schema_phase_exit.rs`

1. **Upload v1** — `namespace acme + entity_type Foo`.
2. **GET active** returns v1 with the canonical entity_type row
   (assert via reading ENTITY_TYPES_TABLE on disk).
3. **Upload v2** — add a predicate; active becomes 2; v1 still
   readable via `SCHEMA_GET(acme, 1)`.
4. **Validate dry-run** of an identical v2 schema → `would_be_version
   = 3` (no persistence side-effect; subsequent LIST still has 2
   entries).
5. **System schema sanity** — `SCHEMA_GET(brain, 0)` returns the
   embedded source text and `schema_version == 1`.

### SDK test

1. `client.schema().upload_text(text)` → outcome with `Some(1)`.
2. `client.schema().validate(text)` → `would_be_version > 0`,
   empty errors.
3. `client.schema().get(ns, 0)` round-trips namespace + version
   + source text.
4. `client.schema().list(ns)` returns 1 entry after a single upload.
5. `SchemaBuilder::new + .entity_type + .build → upload` →
   `outcome.errors.is_empty()`.

## Out of scope

- Concurrent SCHEMA_UPLOADs from multiple clients — single-writer
  per shard already serialises; not exercised explicitly.
- Performance benchmark — phase 19.10b.

## Single commit

`test(brain-server,brain-sdk-rust): 19.10a — schema integration tests`

## Verification

```
just docker cargo test -p brain-server --test knowledge_schema_wire
just docker cargo test -p brain-server --test knowledge_schema_phase_exit
just docker cargo test -p brain-sdk-rust --test knowledge_schema_sdk
cargo clippy --target x86_64-unknown-linux-gnu --workspace --all-targets -- -D warnings
```
