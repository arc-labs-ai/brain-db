# §28 Knowledge Wire Protocol — backfill plan

> **Trigger:** course-correction from phase 16.6 retrospective. §28 was a 1-file stub when we implemented 16.6c entity wire ops — we invented details (rkyv body shapes, `EntityView`, error mapping, `move_to_alias=false` semantics) instead of expanding the spec first. This plan backfills §28 to match `spec/03_wire_protocol/`'s 16-file depth.

## Reference depth (substrate's §03)

```
00_purpose.md           4.1K  — what this section covers
01_design_choices.md    9.1K  — rationale: rkyv, CRC32C, u24 payload, etc.
02_transport.md        10.7K  — TCP/TLS, framing, backpressure
03_frame_header.md     12.2K  — 32-byte layout
04_payload_encoding.md 10.3K  — rkyv conventions + raw blobs
05_opcodes.md          10.4K  — opcode table + dispatch rules
06_handshake.md        11.3K  — HELLO/WELCOME/AUTH/AUTH_OK
07_request_frames.md   12.6K  — per-op request body schemas
08_response_frames.md  12.0K  — per-op response body schemas
09_streaming.md        11.0K  — STREAM_START/ITEM/END
10_errors.md           10.6K  — ErrorCode taxonomy, categories
11_validation.md       11.2K  — field validation rules
12_versioning.md        9.3K  — protocol-version policy
13_open_questions.md    8.3K  — what we don't know yet
14_references.md        5.4K  — cross-links
README.md
```

Knowledge layer doesn't need everything (transport / handshake / frame_header reuse §03). It does need everything else.

## Proposed §28 file structure

```
spec/28_knowledge_wire_protocol/
├── README.md                         (new)
├── 00_purpose.md                     (already updated 16.6a; keep)
├── 01_design_choices.md              (new) — namespace split rationale, u16 opcode, rkyv body, error-frame reuse
├── 02_payload_encoding.md            (new) — knowledge-specific rkyv conventions, large-blob handling
├── 03_schema_frames.md               (new) — SCHEMA_UPLOAD / GET / LIST / VALIDATE / EXTRACTOR_LIST/DISABLE/ENABLE bodies
├── 04_entity_frames.md               (new) — ENTITY_CREATE / GET / UPDATE / RENAME / MERGE / UNMERGE / RESOLVE / LIST / TOMBSTONE bodies
├── 05_statement_frames.md            (new) — STATEMENT_CREATE / GET / SUPERSEDE / TOMBSTONE / RETRACT / HISTORY / LIST bodies
├── 06_relation_frames.md             (new) — RELATION_CREATE / GET / SUPERSEDE / TOMBSTONE / LIST_FROM / LIST_TO / TRAVERSE bodies
├── 07_query_frames.md                (new) — QUERY / QUERY_EXPLAIN / QUERY_TRACE / RECALL_HYBRID bodies + streaming
├── 08_admin_frames.md                (new) — ADMIN_REBUILD_INDEX / REINDEX_TANTIVY / LIST_PENDING_RESOLUTIONS / RESOLVE_AMBIGUITY / GET_AUDIT / LIST_STALE_STATEMENTS / BACKFILL / JOB_STATUS bodies
├── 09_subscribe_events.md            (new) — knowledge event types riding substrate SUBSCRIBE
├── 10_errors.md                      (new) — §28 error code mapping into substrate ERROR frame; per-family error semantics
├── 11_validation.md                  (new) — field-level validation rules (canonical_name length, alias count, attribute size, etc.)
├── 12_schema_optional_mode.md        (new) — gate behavior when no schema declared
├── 13_open_questions.md              (new) — known gaps + decisions deferred to later phases
└── 14_references.md                  (new) — cross-links to §17–§30
```

Total: 1 existing + 14 new = 15 files (substrate is 16; we skip handshake/transport/frame_header since they're inherited).

## Depth bar per file

Each new file must:

- Open with a one-paragraph purpose.
- Be **implementable** — if a developer reads only §28/NN.md, they can write the corresponding code without inventing shapes.
- Include at least one worked example for non-trivial encodings.
- Cross-ref to the upstream domain spec (`spec/18_entities/`, `spec/19_statements/`, etc.).
- For ops not yet implemented (phase 16.7+, phase 17+), describe the wire shape but mark `**Status:** spec-only, code lands phase NN`.

## Sequencing

Three sittings, in priority order:

### Sitting A — what 16.6c implemented + what 16.7+ needs next

Files: `04_entity_frames.md`, `09_subscribe_events.md` (entity events only), `10_errors.md`, `11_validation.md`, `00_purpose.md` cross-ref polish.

Codifies the 16.6c implementation (`EntityCreateRequest` / `EntityView` / etc.) at spec-depth, and lays out the entity merge / unmerge / resolve / list / tombstone shapes that 16.7-16.9 need.

### Sitting B — phase 17–19 prep

Files, in write order with sequential numbers continuing from Sitting A's `04`:

- `05_schema_frames.md` — `0x0120–0x0126` body shapes.
- `06_statement_frames.md` — `0x0140–0x0146` body shapes.
- `07_relation_frames.md` — `0x0150–0x0156` body shapes.
- `08_schema_optional_mode.md` — gate behavior, error semantics, when ops downgrade.
- `09_open_questions.md` — already cross-referenced from Sittings A's files; lands here so refs resolve.
- `10_references.md` — cross-links to §17–§30 domain sections.
- `README.md` — section overview (no leading number; matches §03's pattern).

Detailed-enough that phase 17 (statements) and phase 18 (relations) and phase 19 (schema DSL) can begin spec-first per the new discipline.

### Sitting C — phase 20–24 prep

Files, continuing sequentially from Sitting B's `10`:

- `11_design_choices.md` — namespace split rationale, u16 opcode, rkyv body, ERROR-frame reuse.
- `12_payload_encoding.md` — knowledge-specific rkyv conventions, large-blob handling.
- `13_query_frames.md` — `0x0160–0x0163` body shapes + streaming semantics detail.
- `14_admin_frames.md` — `0x0170–0x0177` body shapes.

`11` and `12` are reference / rationale; the query and admin frames close the wire surface for phases 22–24.

> **Note on filename predictions:** Sitting A's live files cross-reference `./09_open_questions.md` (predicted Sitting B slot 5). Honor that filename when Sitting B lands — write order in Sitting B is fixed at the order above for that reason. Re-ordering Sitting B would require updating Sitting A's refs.

Each sitting is its own commit (or kept uncommitted for batch review). User decides after each sitting whether to continue or pause.

## What this plan does NOT do

- **Does not touch §17–§31 domain sections** (e.g. §18 entities body, §19 statements semantics). Those are separate backfills the [[spec-first-workflow]] memory will catch when phases 17+ start. §28 is just the **wire surface**.
- **Does not change code.** The 16.6c implementation stays as-is; §28 will be edited to match what was implemented (no design changes), with any newly-discovered gaps surfaced as open questions rather than implementation changes.
- **Does not commit.** Same convention you set earlier — write, then manual review, then commit decision.

## Risks

- **Drift.** While I write spec files, the user may want to start 16.7. We should agree: 16.7+ blocked until at least Sitting A is approved.
- **Over-specification.** It's tempting to lock in details for ops far in the future (admin, query); risk is wrong-by-construction text. I'll mark speculative sections explicitly.
- **Citation churn.** Some §17–§27 sections (especially the 1-file stubs) will be referenced from §28 but don't have the targets yet. I'll cite them with `**TBD:** see [[upcoming-section]]` placeholders.

## Approval gates

- This plan → user approves the structure.
- Sitting A → user reviews 5 new files, approves.
- Sitting B → user reviews 7 new files, approves.
- Sitting C → user reviews 4 new files, approves.

## After all three sittings

- Save a follow-up memory entry: "§28 brought to §03 depth; pattern to apply for §17, §19, §20, §22, §24, §26, §27, §29, §31 going forward."
- 16.7 unblocked (its spec dependency, §18 entity merge, is also under-specified — flag for §18-backfill before 16.7 code).
