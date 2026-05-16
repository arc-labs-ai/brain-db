# 25.08 References

Cross-links from §25 to the rest of the spec.

## Sibling knowledge-layer sections

| Target | §25 file referencing |
|---|---|
| [`../17_knowledge_model/00_purpose.md`](../17_knowledge_model/00_purpose.md) | Three-layer model — provenance binds layers together. |
| [`../19_statements/04_confidence.md`](../19_statements/04_confidence.md) | Confidence aggregation that consumes per-evidence weights from extraction. |
| [`../22_extractors/05_audit.md`](../22_extractors/05_audit.md) | Extractor audit row. |
| [`../22_extractors/06_idempotency.md`](../22_extractors/06_idempotency.md) | Replay semantics keyed off audit rows. |
| [`../27_knowledge_workers/01_extractor_workers.md`](../27_knowledge_workers/01_extractor_workers.md) | Workers writing audit rows. |

## Substrate dependencies

| Target | §25 file referencing |
|---|---|
| [`../05_storage_arena_wal/`](../05_storage_arena_wal/) | WAL durability for audit-row writes. |
| [`../07_metadata_graph/`](../07_metadata_graph/) | redb table layout. |

## §25 internal file map

For navigation:

| File | Purpose |
|---|---|
| [`./00_purpose.md`](./00_purpose.md) | Overview — provenance invariants, retention, re-extraction. |
| [`./01_audit_tables.md`](./01_audit_tables.md) | redb table layout for `EXTRACTOR_AUDIT_TABLE` + indexes. |
| [`./07_open_questions.md`](./07_open_questions.md) | Deferrals. |
| [`./08_references.md`](./08_references.md) | This file. |

**Other provenance topics** (FORGET cascade, confidence
aggregation, stale-extraction detection, version visibility,
retention) live as sections inside [`./00_purpose.md`](./00_purpose.md);
each will get its own dedicated file when implementation depth
demands it (phase 22+).
