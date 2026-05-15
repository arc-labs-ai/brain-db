# §28 — Knowledge Wire Protocol

Wire shapes and dispatch semantics for the knowledge-layer opcodes (the `0x01xx` namespace of the u16 wire opcode). The substrate's wire foundation — transport, handshake, frame header, ERROR / SUBSCRIBE primitives — lives in [`../03_wire_protocol/`](../03_wire_protocol/) and is reused verbatim.

## Where to start

- New to §28? Read [`./00_purpose.md`](./00_purpose.md) first (opcode tables + namespace overview).
- Implementing a specific opcode? Open the matching `NN_*_frames.md` file:
  - [`./01_entity_frames.md`](./01_entity_frames.md) — entity ops (`0x0130–0x0138`).
  - [`./05_schema_frames.md`](./05_schema_frames.md) — schema + extractor governance (`0x0120–0x0126`).
  - [`./06_statement_frames.md`](./06_statement_frames.md) — statement ops (`0x0140–0x0146`).
  - [`./07_relation_frames.md`](./07_relation_frames.md) — relation ops (`0x0150–0x0156`).
  - `13_query_frames.md` (TBD, Sitting C) — query ops (`0x0160–0x0163`).
  - `14_admin_frames.md` (TBD, Sitting C) — admin ops (`0x0170–0x0177`).
- Wiring errors? [`./03_errors.md`](./03_errors.md).
- Field caps and validation? [`./04_validation.md`](./04_validation.md).
- SUBSCRIBE events emitted by knowledge ops? [`./02_subscribe_events.md`](./02_subscribe_events.md).
- Schema-not-declared mode? [`./08_schema_optional_mode.md`](./08_schema_optional_mode.md).
- Cross-links and code paths? [`./10_references.md`](./10_references.md).

## Conventions

- **Numbering is write-order, no gaps.** Sittings A (files 01–04) and B (files 05–10) landed; Sitting C (files 11–14) is pending. See [`../../.claude/plans/phase-28-backfill.md`](../../.claude/plans/phase-28-backfill.md).
- Each opcode section has the same structure: request body, response body, error responses, examples / cross-shard notes.
- All structs derive `Archive + Serialize + Deserialize + check_bytes` (rkyv 0.7). `check_bytes` is mandatory.
- Optional `WireUuid` fields use the `[0u8; 16]` sentinel rather than `Option<[u8; 16]>` — UUIDv7's first 48 bits are a timestamp so the all-zero value is unrepresentable as a real id.

## Status snapshot

As of phase 16.6c:

- **Implemented (round-trip tests in `crates/brain-protocol/`):** `ENTITY_CREATE`, `ENTITY_GET`, `ENTITY_UPDATE`, `ENTITY_RENAME`.
- **Spec-only, wired in upcoming phase:** every other knowledge opcode. Per-section status tables list the phase.

## Phase 16.6a wire change

The opcode widened from `u8` to `u16` and `flags` shrank from `u16` to `u8`. Substrate ops kept their byte values (`ENCODE_REQ = 0x0020`). Knowledge ops live at high byte `0x01`. See [`../03_wire_protocol/12_versioning.md`](../03_wire_protocol/12_versioning.md) §0 for the pre-v1.0 compatibility policy that licensed the change, and [`./09_open_questions.md`](./09_open_questions.md) §"Resolved" for the resolution trail.

## File map

| File | Purpose |
|---|---|
| [`00_purpose.md`](./00_purpose.md) | Opcode tables, families, error-code table, schema-optional mode. |
| [`01_entity_frames.md`](./01_entity_frames.md) | Body shapes for `0x0130–0x0138` entity ops. |
| [`02_subscribe_events.md`](./02_subscribe_events.md) | Knowledge event types riding substrate SUBSCRIBE. |
| [`03_errors.md`](./03_errors.md) | §28 error code mapping into substrate ERROR frame. |
| [`04_validation.md`](./04_validation.md) | Field-level validation rules per opcode. |
| [`05_schema_frames.md`](./05_schema_frames.md) | Body shapes for `0x0120–0x0126` schema / extractor-governance ops. |
| [`06_statement_frames.md`](./06_statement_frames.md) | Body shapes for `0x0140–0x0146` statement ops. |
| [`07_relation_frames.md`](./07_relation_frames.md) | Body shapes for `0x0150–0x0156` relation ops. |
| [`08_schema_optional_mode.md`](./08_schema_optional_mode.md) | Gate behavior when no schema declared. |
| [`09_open_questions.md`](./09_open_questions.md) | Known gaps + decisions deferred. |
| [`10_references.md`](./10_references.md) | Cross-links to §17–§30 + substrate. |
| `11_design_choices.md` | (Sitting C) Rationale for namespace split, u16 opcode, etc. |
| `12_payload_encoding.md` | (Sitting C) Knowledge-specific rkyv conventions, large-blob handling. |
| `13_query_frames.md` | (Sitting C) `0x0160–0x0163` body shapes + streaming detail. |
| `14_admin_frames.md` | (Sitting C) `0x0170–0x0177` body shapes. |
