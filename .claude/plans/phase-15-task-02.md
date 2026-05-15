# Sub-task 15.2 — Knowledge-layer WAL frame kind discriminator

> Per-sub-task plan. Plan-first convention.

## Goal

Extend the WAL framing layer to recognize the 12 knowledge-layer record kinds defined in `spec/26_knowledge_storage/00_purpose.md`. After this sub-task:

- `WalRecordKind` enum accepts the new kinds (discriminants `0x10..0x50`).
- `WalPayload` can carry a knowledge record (as an opaque body for now).
- The writer can produce a frame with any of the new kinds.
- The reader can step over a knowledge frame without breaking.
- Recovery treats knowledge records as no-ops (knowledge state hydration is implemented in phases 16–19).
- Substrate frame round-trip remains 100% identical.

Aligns with phase doc 15.2; the typed body schemas land in phases 16 (entity), 17 (statement), 18 (relation), 19 (schema), 20+ (audit). 15.2 is the *framing* extension only.

## Reading list

1. `spec/26_knowledge_storage/00_purpose.md` — "WAL frame types" section.
2. `spec/05_storage_arena_wal/05_wal_records.md` — current record-header format (unchanged).
3. `crates/brain-storage/src/wal/kinds.rs` — substrate enum (15 variants today, 1..=15).
4. `crates/brain-storage/src/wal/payload.rs` — `WalPayload` enum + `kind()` / `encode_to_bytes()` / `decode()` dispatch.
5. `crates/brain-storage/src/wal/record.rs` — frame header layout, `WalRecord::from_typed`, encode/decode.
6. `crates/brain-storage/src/recovery.rs` — `match WalPayload::*` arms (no-op the new kinds).

## Pre-flight findings

### F-1 — Discriminant range lines up

Spec §26's hex `0x10..0x50` is decimal `16..=80`. Substrate uses `1..=15`. They are disjoint; the kinds.rs negative test (`from_u8(16) == None`) needs to be relaxed to assert against a still-reserved value (e.g. `96` — between `Audit=0x50` and the v2 reserved boundary `0x80=128`).

### F-2 — Knowledge kind ↔ discriminant mapping

12 new kinds, decimal values:

| Spec name | Variant | Discriminant |
|---|---|---|
| `0x10 ENTITY_CREATE` | `EntityCreate` | 16 |
| `0x11 ENTITY_UPDATE` | `EntityUpdate` | 17 |
| `0x12 ENTITY_MERGE` | `EntityMerge` | 18 |
| `0x13 ENTITY_TOMBSTONE` | `EntityTombstone` | 19 |
| `0x20 STATEMENT_CREATE` | `StatementCreate` | 32 |
| `0x21 STATEMENT_SUPERSEDE` | `StatementSupersede` | 33 |
| `0x22 STATEMENT_TOMBSTONE` | `StatementTombstone` | 34 |
| `0x30 RELATION_CREATE` | `RelationCreate` | 48 |
| `0x31 RELATION_SUPERSEDE` | `RelationSupersede` | 49 |
| `0x32 RELATION_TOMBSTONE` | `RelationTombstone` | 50 |
| `0x40 SCHEMA_UPDATE` | `SchemaUpdate` | 64 |
| `0x50 AUDIT` | `Audit` | 80 |

Gaps (`20..=31`, `35..=47`, `51..=63`, `65..=79`, `81..=127`) remain reserved within the knowledge-layer block. `128..` reserved for v2 (unchanged).

### F-3 — `WalPayload` is the impact surface

`WalPayload` enum has 15 variants today plus dispatch in three places (`kind()`, `encode_to_bytes()`, `decode()`). 12 new variants × 3 dispatch sites = 36 new arms — wasteful for opaque placeholders.

**Recommended approach (D2 below):** add a single `Knowledge(KnowledgeRecord)` variant where `KnowledgeRecord { kind: WalRecordKind, body: Vec<u8> }`. One variant covers all 12 new kinds with opaque bodies; phases 16–19 replace the opaque body with typed variants per kind as they implement them.

### F-4 — Recovery dispatch

`recovery.rs` matches on `WalPayload` to drive substrate state. Adding `Knowledge(_)` requires one new arm: no-op (`continue` in the replay loop). Knowledge state hydration is a phase-16+ concern that adds its own replay path.

### F-5 — `decode()`'s trailing-bytes check

`WalPayload::decode` asserts no trailing bytes after structured-field consumption. For the `Knowledge` variant, the entire body IS the opaque payload — the reader consumes nothing structured, so the check would fire. The arm must skip the trailing-bytes assertion and copy all bytes into `body`.

## Design decisions

### D1 — Add 12 new variants to `WalRecordKind`

Discriminants per F-2 table. Update:
- The `#[repr(u8)]` enum body.
- `from_u8` arms.
- `as_u8` works automatically (repr(u8) cast).
- `ALL_KINDS` slice.
- Negative test: `from_u8(96)` (reserved within knowledge block) and `from_u8(128)` (v2 boundary) both return `None`. Drop the stale `from_u8(16) == None` assertion; replace with `from_u8(16) == Some(EntityCreate)`.

### D2 — Single `Knowledge` variant in `WalPayload` (opaque body)

```rust
pub struct KnowledgeRecord {
    pub kind: WalRecordKind,  // one of the 12 knowledge kinds
    pub body: Vec<u8>,        // opaque rkyv-encoded body; typed in later phases
}

pub enum WalPayload {
    // … existing 15 variants …
    Knowledge(KnowledgeRecord),
}
```

`KnowledgeRecord::new(kind, body)` validates `kind` is in the knowledge range (`16..=80`); a non-knowledge kind is a programmer error → debug_assert + Err return path.

`WalPayload::kind()`: `Self::Knowledge(r) => r.kind`.

`WalPayload::encode_to_bytes()`: `Self::Knowledge(r) => out.extend_from_slice(&r.body)`.

`WalPayload::decode(kind, bytes)`:
- If `kind` is a substrate kind (1..=15): existing arms.
- If `kind` is a knowledge kind (16..=80 matching an enum variant): `Self::Knowledge(KnowledgeRecord { kind, body: bytes.to_vec() })`.
- Otherwise: `WalPayloadError::Unknown(kind)` (new error variant).

The trailing-bytes check is skipped on the Knowledge arm.

### D3 — Recovery no-ops knowledge records

`recovery.rs`'s replay loop gets one new match arm: `WalPayload::Knowledge(_) => { /* no-op in substrate replay; knowledge-state hydration lands in phases 16+ */ }`. Log at trace level so it's visible in audit but invisible at info+.

### D4 — Bound `KnowledgeRecord::body` size

Frame header carries a 3-byte `payload_len` (per spec §03/05) — soft cap ~16 MB. The Knowledge variant should NOT impose a stricter cap; substrate frames already share this bound. Document the bound in the struct doc-comment.

### D5 — Tests

In `kinds.rs`:
- Update `discriminants_match_spec_table` to spot-check the new boundaries (16, 32, 48, 64, 80).
- Update `from_u8_round_trips_every_kind` (`ALL_KINDS` covers them).
- Replace `from_u8_rejects_reserved_and_unknown` assertions: `from_u8(15+1=16)` is now valid; new negative cases are `20` (gap inside entity block), `96` (gap inside knowledge block), `128` (v2 boundary), `255`.
- `all_kinds_covers_every_variant`: update count from 15 → 27.

In `payload.rs`:
- New test `knowledge_record_round_trip` that builds a `KnowledgeRecord` with a random body, encodes via `WalPayload::Knowledge`, decodes back, asserts kind+body equal.
- New test `decode_unknown_kind_errors` that supplies a discriminant byte outside the legal set (e.g., 96) and expects `WalPayloadError::Unknown`.

In `record.rs`:
- New test `frame_round_trip_for_knowledge_kind` that runs the full `from_typed → encode → decode → typed_payload` cycle for a `KnowledgeRecord`. Mirrors the existing `round_trip_typed_payload` test.

In `recovery.rs`:
- New test `recovery_skips_knowledge_records` that writes a mixed WAL (substrate Encode + Knowledge + substrate Encode) and asserts the replay applies only the two substrate records.

## File plan

- `crates/brain-storage/src/wal/kinds.rs` — add 12 variants + update tests.
- `crates/brain-storage/src/wal/payload.rs` — add `KnowledgeRecord` struct + `Knowledge` variant + dispatch + new error variant + tests.
- `crates/brain-storage/src/wal/record.rs` — add round-trip test for knowledge frame.
- `crates/brain-storage/src/recovery.rs` — add no-op match arm + new test.

No new crates, no new dependencies.

## Done-when

- `cargo zigbuild -p brain-storage --tests --target x86_64-unknown-linux-gnu` clean.
- All new tests written; existing tests stay green.
- A WAL containing a mix of substrate + knowledge records replays without applying knowledge records to substrate state.
- Substrate-only WAL behavior is byte-identical to pre-change (same frame format; existing tests unaffected).
- One commit: `feat(storage): 15.2 — knowledge-layer WAL frame kind discriminator`.

## Risk register

| Risk | Mitigation |
|---|---|
| Adding `Knowledge` variant changes WAL on-disk schema version | It doesn't — the frame header is unchanged; new `record_type` byte values were already reserved per spec §05/05 §3. No `format_version` bump. |
| Forgetting a `match WalPayload::*` site elsewhere | Grep `match.*WalPayload` across the workspace; verify each arm explicitly. Compiler's exhaustiveness check will catch missing arms at build time. |
| Recovery silently ingests a knowledge record as substrate state | The new `Knowledge(_) => no-op` arm + recovery test guard this. |
| `WalPayloadError::Unknown` adds a new error variant that callers may not handle | Audit `match WalPayloadError` sites; existing handlers use `_ =>` catch-alls or chain via `?`. Confirm before adding. |
| Negative-test discriminant choice collides with future spec additions | Use a value clearly in a reserved gap (96); add a comment pointing to spec §26 frame-type table. |

## Open questions for your approval

1. **Variant strategy (D2)** — single `Knowledge(KnowledgeRecord)` with opaque body, OR 12 typed-but-empty variants? **Recommended: single variant.** Phases 16–19 replace it with typed variants per kind as they implement those kinds; no churn in 15.2.
2. **Recovery logging (D3)** — `trace!` (silent at default verbosity), `debug!`, or `info!` when replay encounters a knowledge record? **Recommended: `trace!`** — recovery sees these on every restart once knowledge is active; info would flood logs.
3. **`WalPayloadError::Unknown(u8)`** — is adding this variant OK, or would you prefer reusing an existing error (e.g., `BadMemoryKind` is the closest)? **Recommended: new variant.** `BadMemoryKind` is semantically wrong; `Unknown(u8)` is honest about what happened.

## Workflow

On your nod: implement, run `cargo zigbuild -p brain-storage --tests --target x86_64-unknown-linux-gnu`, commit as `feat(storage): 15.2 — knowledge-layer WAL frame kind discriminator`, then stop and write the 15.3 plan.
