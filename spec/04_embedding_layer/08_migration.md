# 04.08 Embedding Migration

The procedure for changing the embedding model on a running deployment. This is the operational playbook; the cognitive operations spec covers the wire interface (`ADMIN_MIGRATE_EMBEDDINGS`).

## 1. Why migrations happen

Operators change the embedding model when:

- A better model is released (BGE upgrades, new model families).
- The deployment's needs change (multilingual, domain-specific).
- A bug is discovered in the current model (poor performance on specific input types).

Migrations are infrequent but consequential. The procedure protects against data loss and minimizes downtime.

## 2. The migration model

Conceptually, migration is:

1. Load the new model alongside the old.
2. For each memory, re-embed its text with the new model.
3. Update the memory's vector and fingerprint.
4. Atomically swap the active model.

In practice, this is more careful — the substrate stays available throughout, and the migration is resumable.

## 3. Pre-migration checks

Before starting a migration, the operator verifies:

- **Disk space** — re-embedding produces new vectors; if the new model has higher dimensionality, additional storage is needed. The substrate's `ADMIN_STATS` reports the projected disk usage.
- **Model compatibility** — the new model's output dimensionality. If it differs from the current dim, the arena needs to be rebuilt with new slot size (a heavier operation).
- **Backup** — a snapshot taken before migration ensures the old state is recoverable. Always recommended.

## 4. Same-dim migration

If the new model produces vectors of the same dimensionality (e.g., another 384-dim model), migration is in-place:

1. **Phase 1: Setup.** Operator deploys the new model alongside the old. Both fingerprints are registered in the substrate's fingerprint table.
2. **Phase 2: Migration runs.** Background worker re-embeds memories one shard at a time, batch by batch.
3. **Phase 3: Cutover.** Once all memories are re-embedded, the operator updates configuration to use the new model as the active one. Old fingerprint is retired.
4. **Phase 4: Cleanup.** Old model files are removed from disk.

Throughout, the substrate is queryable. Queries embed with whichever model is currently active. As migration proceeds, more memories match the active fingerprint; query coverage increases over time.

## 5. Different-dim migration

If the new model has a different output dimensionality, the arena's slot size must change. This is heavier:

1. **Phase 1: Setup.** Deploy new model. Allocate a new arena with the new slot size, alongside the existing one.
2. **Phase 2: Migration.** Background worker re-embeds each memory and writes the new vector to the new arena. The old arena and old vectors stay in place.
3. **Phase 3: Index rebuild.** New HNSW index is built against the new arena.
4. **Phase 4: Cutover.** Configuration is updated; new arena and index become active.
5. **Phase 5: Cleanup.** Old arena and index are deleted.

This is online — the substrate is queryable throughout — but it requires roughly 2× disk space during the migration window.

## 6. The migration worker

A background worker performs the actual re-embedding:

```
loop {
    for memory in next_batch_of_unmigrated_memories(shard) {
        let text = read_memory_text(memory.id);
        let new_vector = new_embedding_model.embed(text);
        let new_vector_normalized = normalize(new_vector);

        write_to_new_arena(memory.id, new_vector_normalized);
        update_metadata(memory.id, new_fingerprint);
        atomically_publish_new_vector(memory.id);
    }
}
```

The worker:

- Processes memories in batches (typically 100–1000 at a time).
- Pauses periodically to let request-serving cores breathe.
- Logs progress and exposes it via `ADMIN_STATS`.
- Is resumable — interruption doesn't lose progress (each memory's migration is atomic).

## 7. Rate limiting

The migration worker can be rate-limited to avoid impacting query performance:

```
[migration]
embeddings_per_second = 100   # default; tune for the deployment
```

At 100/s on a single shard, a 1M-memory shard takes ~3 hours. Faster rates risk competing with request-serving for CPU.

If GPU is available, migration uses it (with batching) for much higher throughput — typically 10K/s, completing a 1M-memory migration in ~2 minutes.

## 8. Atomic per-memory updates

Each memory's migration is atomic:

1. New vector written to a temporary location.
2. Metadata updated to point at the new location and update fingerprint.
3. Old vector freed (or marked for cleanup if same-arena migration).

The atomicity ensures that at any instant:

- A query against this memory sees either the old vector (with old fingerprint) or the new (with new fingerprint).
- Never sees a mismatched pair (old vector with new fingerprint, etc.).

The mechanism uses the same epoch-based publication as encode (see [10. Concurrency + Epoch Model](../10_concurrency_epochs/)).

## 9. Query behavior during migration

A query embedded with the new (active) model:

- Matches memories with the new fingerprint.
- Does not match memories with the old fingerprint (cross-fingerprint exclusion).

During migration, the matching subset grows as more memories are re-embedded. Query coverage might be 0% at the start, 50% at the midpoint, 100% at completion.

For operationally-sensitive deployments, two strategies:

### 9.1 Migrate before swapping

Run `ADMIN_MIGRATE_EMBEDDINGS` while the old model is still active. The migration produces new-fingerprint vectors as a side effect, but queries continue to use the old model. Once migration is complete, the operator swaps the active model — at which point all memories already have the new fingerprint, and queries see 100% coverage immediately.

This is the recommended approach.

### 9.2 Tolerate partial coverage

For deployments where the migration window is brief and partial coverage is acceptable, just swap the model and let migration catch up. The first few minutes have reduced query results.

## 10. Failure handling

### 10.1 Crash during migration

The migration worker's progress is persisted (per-memory atomic updates plus a "migration cursor" in metadata). On restart, the worker resumes from the cursor.

Memories already migrated stay migrated; memories not yet processed are processed next.

### 10.2 New model fails to load

If the new model can't be loaded (corrupted weights, configuration error), migration aborts and reports the failure. The substrate continues running with the current model.

### 10.3 Re-embedding fails for specific memories

If embedding a specific memory's text fails (e.g., the text is corrupted or contains characters the new model can't handle), the migration:

- Logs the failure with the memory ID.
- Marks the memory as "migration-deferred".
- Continues with other memories.

After migration completes, deferred memories can be addressed individually (re-encoded by the agent, hard-deleted, or re-tried with operator intervention).

### 10.4 Disk full during different-dim migration

A different-dim migration may need 2× disk space. If the substrate runs out:

- Migration pauses.
- An admin alarm fires.
- The operator addresses the disk situation (add space, delete old data, abort migration).
- Migration resumes once space is available.

## 11. Aborting a migration

The operator can abort an in-progress migration via `ADMIN_ABORT_MIGRATION`. Behavior:

- Same-dim migration: half-migrated memories have the new fingerprint and the new vector; the other half have old fingerprint and old vector. Configuration determines which the active model is. The operator can re-start migration with the same or a different new model.

- Different-dim migration: rolling back means deleting the new arena, freeing the disk space. Memories that were updated to point at the new arena are rolled back to the old. This is invasive and slow; the substrate provides a tool but warns it's a heavy operation.

Aborting is rare. The common path is "let it finish".

## 12. The fingerprint registry

When a new model is registered (before its first encode), the substrate adds it to the per-shard fingerprint registry:

```sql
INSERT INTO model_fingerprints
VALUES (new_fp, 'bge-base-en-v1.5', timestamp_now, vector_dim_768)
```

The registry persists across restarts. Migrations leave the old fingerprint in the registry (it's part of history); a separate `ADMIN_RETIRE_FINGERPRINT` operation removes it after migration completes and the operator confirms no memories carry it.

## 13. Cross-version migrations

Migrations between major model families (e.g., switching from BGE to a multilingual model) are supported by the same machinery as same-family upgrades. The substrate doesn't care about model lineage; it only cares about fingerprints and dimensions.

The risk: very different models may produce dramatically different retrieval quality. Operators should test on a development environment before running the migration in production.

## 14. The end state

After a successful migration:

- All memories carry the new fingerprint.
- The old fingerprint is retired (or kept as historical reference).
- The new model is the active embedder for all subsequent operations.
- The substrate's behavior is identical to a fresh deployment with the new model from day one.

There's no "lingering" effect from the old model. The substrate is in a clean state, ready for normal operation.

---

*Continue to [`09_failure_modes.md`](09_failure_modes.md) for embedding-layer failure modes.*
