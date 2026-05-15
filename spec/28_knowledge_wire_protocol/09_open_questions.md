# 28.09 Open Questions

Known gaps and decisions deferred to future phases. Each entry has a **rough-target phase** for resolution and a **status**. New items are added with a phase number; resolved items move to the bottom under §"Resolved".

## Active

### Q1 — Strategy A vs B for error-code wire shape

[`./03_errors.md`](./03_errors.md) describes two strategies for surfacing knowledge error codes in the substrate ERROR frame:

- **Strategy A** — extend substrate `ErrorCodeWire` with new variants. Long-term plan.
- **Strategy B** — interim fallback that maps knowledge errors onto closest existing substrate codes. Currently in code (phase 16.6c).

Question: when does the substrate's [`../03_wire_protocol/10_errors.md`](../03_wire_protocol/10_errors.md) get extended with the §28 codes?

**Target:** phase 17 (before statement ops land, so Strategy A is available to them). **Status:** open.

---

### Q2 — Schema fan-out coordination

[`./08_schema_optional_mode.md`](./08_schema_optional_mode.md) §6 lists two coordination strategies for multi-shard schema upload:

- Authoritative shard 0 with replicated reads.
- 2PC across shards.

Question: which strategy does phase 19 implement?

**Target:** phase 19. **Status:** open. **Decision criterion:** target consistency window (≤ 100ms for option 1; instant for option 2 but with failure-mode complexity).

---

### Q3 — Cross-type entity merge

[`./01_entity_frames.md`](./01_entity_frames.md) §7.4 — should merging entities of different `entity_type_id` be allowed? Default v1.0 stance: forbidden (returns `ENTITY_TYPE_MISMATCH`).

If allowed in a later version: how are type-specific attributes resolved? Drop them? Migrate to the survivor type?

**Target:** phase 18 (after relation cardinality lands, which will clarify type semantics). **Status:** open. **Likely outcome:** stay forbidden in v1.0.

---

### Q4 — Retroactive event emission for pre-existing entities

[`./02_subscribe_events.md`](./02_subscribe_events.md) §6: phase 16.7 wires entity-event emission. Should it retroactively emit `ENTITY_CREATED` for entities created via the **phase-16.6c** `ENTITY_CREATE` opcode (before event emission was wired)?

Default leaning: **no** — events are forward-only from their introduction. Clients that need a backfill use `ENTITY_LIST`.

**Target:** phase 16.7. **Status:** open. **Likely outcome:** no retroactive emission.

---

### Q5 — `move_to_alias = false` rename semantics

[`./01_entity_frames.md`](./01_entity_frames.md) §6: the `move_to_alias` flag is wire-stable but the handler currently rejects `false`. What does a "no-trail" rename mean semantically?

Two interpretations:

- (a) **Drop the old name entirely.** New canonical, no alias trail. Loses query-ability by old name.
- (b) **Move to alias but flag the alias as deprecated.** Carries a tombstone-style flag on the alias entry.

**Target:** phase 17 or later. **Status:** open. **Likely outcome:** (b), but not in v1.0.

---

### Q6 — Aliases and Unicode confusables

[`./04_validation.md`](./04_validation.md) §7: aliases are deduplicated on `normalize_name` (lowercase + whitespace collapse). Should that also apply NFKC normalization to collapse Unicode confusables (e.g. Cyrillic "а" vs Latin "a")?

**Target:** phase 17 (alongside statement-level text validation). **Status:** open. **Trade-off:** correctness vs surprise (NFKC can alter visible characters).

---

### Q7 — `attributes_blob` schema-aware validation on the wire

[`./04_validation.md`](./04_validation.md) §7: `attributes_blob` is opaque at the wire layer. Should phase 19 push schema validation **into** the wire layer (decode the blob, check against the entity type's attribute schema, reject malformed before the handler runs)?

**Target:** phase 19. **Status:** open. **Trade-off:** cleaner errors at wire-time vs more upfront decode cost on hot paths.

---

### Q8 — Streaming back-pressure semantics for `ENTITY_LIST` / `STATEMENT_LIST` / `RELATION_LIST_*`

Substrate streaming back-pressure ([`../03_wire_protocol/09_streaming.md`](../03_wire_protocol/09_streaming.md)) applies, but the **knowledge-specific** detail: when the substrate's per-stream buffer fills, do the knowledge list operations:

- (a) **Block** the producer (current substrate convention)?
- (b) **Cancel** the stream with a partial result + cursor for resume (knowledge-specific)?

Default leaning: (a) until a real workload proves otherwise.

**Target:** phase 23 (when query streaming hits production scale). **Status:** open.

---

### Q9 — Schema removal (`SCHEMA_DROP`)

[`./08_schema_optional_mode.md`](./08_schema_optional_mode.md) §1 — once a schema is declared, there's no opcode to revert. Should a `SCHEMA_DROP` opcode exist?

**Trade-off:** Symmetric API vs accidental-erasure risk. A deployment with 10M statements losing schema would orphan them entirely.

**Target:** post-v1.0. **Status:** deferred. **Likely outcome:** never as a wire opcode; only as an offline admin action.

---

### Q10 — Streaming `STREAM_START` envelope vs reuse of substrate streaming

[`./00_purpose.md`](./00_purpose.md) §"Streaming responses" — §28 originally referenced `STREAM_START` / `STREAM_ITEM` / `STREAM_END` framing. The substrate has no such envelope; streaming is per-frame with shared `stream_id` and EOS on the last frame.

The §28 prose has been updated to reuse substrate streaming verbatim. This is the **final** decision; the original `STREAM_START` references in earlier drafts are obsolete. Listed here so anyone finding stale notes knows it was resolved.

**Status:** resolved (phase 16.6a). **Outcome:** reuse substrate streaming, no §28-specific envelope.

## Resolved

### R1 — Opcode namespace conflict between §03 and §28

Phase 16.6a. The pre-rewrite §28 opcode table directly collided with the substrate's §03/05 opcode assignments (`0x30` was `ENTITY_CREATE` in §28 but `SUBSCRIBE_REQ` in §03). Resolved by widening the wire opcode to `u16` and splitting into namespace bytes (`0x00xx` substrate, `0x01xx` knowledge). See [`./00_purpose.md`](./00_purpose.md) and [`../03_wire_protocol/05_opcodes.md`](../03_wire_protocol/05_opcodes.md).

### R2 — Flags field shrunk to u8

Phase 16.6a. The pre-rewrite §03/03 header had a 16-bit `flags` field with bits 12-0 reserved. Shrunk to `u8` (only EOS / MPL / CMP bits ever used) and reclaimed the freed byte for the upper half of the new `u16` opcode. See [`../03_wire_protocol/03_frame_header.md`](../03_wire_protocol/03_frame_header.md).

### R3 — pre-v1.0 compatibility policy

Phase 16.6a. The opcode-width change in R1 / flags shrink in R2 are non-backward-compatible wire changes. Both are permitted under the pre-v1.0 compatibility policy documented in [`../03_wire_protocol/12_versioning.md`](../03_wire_protocol/12_versioning.md) §0.

### R4 — `STREAM_START` envelope (see Q10 above)

Marked resolved here as well — substrate streaming is reused.
