# 11.11 References

References for the background workers.

## 1. Memory consolidation in cognitive science

- **McClelland, McNaughton, O'Reilly, "Why there are complementary learning systems in the hippocampus and neocortex" (1995, Psychological Review).** [psychnet.apa.org](https://psycnet.apa.org/doi/10.1037/0033-295X.102.3.419). The classic paper on complementary learning systems and consolidation.

- **Squire & Alvarez, "Retrograde amnesia and memory consolidation" (1995).** Earlier theoretical framework.

These are inspiration for the abstraction but Brain doesn't claim cognitive fidelity.

## 2. Salience and decay

- **Anderson & Schooler, "Reflections of the environment in memory" (1991, Psychological Science).** [doi.org/10.1111/j.1467-9280.1991.tb00174.x](https://doi.org/10.1111/j.1467-9280.1991.tb00174.x). The "rational analysis" of memory; provides theoretical justification for decay.

- **Wickelgren, "Single-trace fragility theory of memory" (1974).** Power-law / exponential decay models.

## 3. Background work in databases

- **PostgreSQL's vacuum and autovacuum** — [postgresql.org/docs/current/routine-vacuuming.html](https://www.postgresql.org/docs/current/routine-vacuuming.html). The most widely-deployed example of background table maintenance.

- **RocksDB's compaction** — [github.com/facebook/rocksdb/wiki/Compaction](https://github.com/facebook/rocksdb/wiki/Compaction). LSM compaction is the analog.

## 4. The "soft real-time" worker model

- **Liu, "Real-Time Systems" (textbook).** Background on soft-real-time scheduling concepts.

## 5. LRU cache design

- **`lru` Rust crate** — [github.com/jeromefroe/lru-rs](https://github.com/jeromefroe/lru-rs).

- **Wikipedia: Cache replacement policies** — [en.wikipedia.org/wiki/Cache_replacement_policies](https://en.wikipedia.org/wiki/Cache_replacement_policies).

## 6. WAL retention patterns

- **PostgreSQL's WAL retention** — [postgresql.org/docs/current/wal-configuration.html](https://www.postgresql.org/docs/current/wal-configuration.html). Authoritative reference.

- **MySQL's binlog retention** — analogous patterns.

## 7. Idempotency cleanup

- **Stripe's idempotency key documentation** — [stripe.com/docs/api/idempotent_requests](https://stripe.com/docs/api/idempotent_requests). They use a similar TTL pattern.

## 8. Adjacent reading

- **Kleppmann, "Designing Data-Intensive Applications" (2017).** Chapter on storage and retrieval covers compaction-like operations.

- **Petrov, "Database Internals" (2019).** Chapters on B-tree storage and LSM compaction relevant to the underlying engines we use.

## 9. Brain-internal references

- See [02.07 Salience](../02_data_model/07_salience.md) for the salience model.
- See [02.08 Lifecycle](../02_data_model/08_lifecycle.md) for memory lifecycle.
- See [05.05 Slot Lifecycle](../05_storage_arena_wal/05_slot_lifecycle.md) for slot reclamation.
- See [06.07 Maintenance](../06_ann_index/07_maintenance.md) for HNSW maintenance specifics.
- See [07.06 Idempotency](../07_metadata_graph/06_idempotency.md) for idempotency table.
