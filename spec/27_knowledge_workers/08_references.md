# 27.08 References

Cross-links from §27 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §27 file referencing |
|---|---|
| [`../22_extractors/`](../22_extractors/) | Pattern / classifier / LLM extractor semantics. |
| [`../18_entities/01_resolution.md`](../18_entities/01_resolution.md) | Entity resolver tiers. |
| [`../19_statements/04_confidence.md`](../19_statements/04_confidence.md) | Confidence aggregation that a future decay sweeper recomputes. |
| [`../25_provenance_versioning/01_audit_tables.md`](../25_provenance_versioning/01_audit_tables.md) | Audit tables the extractor workers write. |

## Substrate dependencies

| Target | §27 file referencing |
|---|---|
| [`../11_workers/`](../11_workers/) | Substrate worker scheduling discipline shared with knowledge-layer workers. |
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | WAL durability for the audit-row writes that workers perform. |
| [`../09_cognitive_operations/02_encode.md`](../09_cognitive_operations/02_encode.md) | ENCODE op dispatches pattern extractors synchronously and classifier extractors via the near-foreground queue. |

## §27 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Worker overview + per-worker table. |
| [`./01_extractor_workers.md`](./01_extractor_workers.md) | Pattern / classifier / LLM worker tiers. |
| [`./07_open_questions.md`](./07_open_questions.md) | Deferrals. |
| [`./08_references.md`](./08_references.md) | This file. |

**Other workers** (decay sweeper / FORGET cascade / resolution
workers / audit log sweeper / schema migration / entity GC /
ambiguity resolver / stale extraction detection) live as one-line
entries in [`./00_purpose.md`](./00_purpose.md)'s table; each will
get its own dedicated file when implementation lands (phase 22+).
