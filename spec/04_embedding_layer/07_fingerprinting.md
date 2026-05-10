# 04.07 Model Fingerprinting

A **model fingerprint** is a 16-byte identifier for a (model, version, configuration) tuple. It propagates through the system: every memory carries the fingerprint of the model that produced its vector. This file specifies the fingerprint and how it's used.

## 1. Why fingerprint

When the embedding model changes, vectors produced by the new model are not comparable to vectors produced by the old. A query embedded with the new model and compared against an old-model memory by dot product produces noise — the geometry of the two vector spaces is different.

The substrate prevents this by tagging every vector with its source model's fingerprint and refusing to compare across fingerprints.

Without fingerprinting, model upgrades would either:

- Silently degrade query quality (the substrate doesn't know it has stale vectors).
- Require manual coordination to re-embed everything before switching.

With fingerprinting, the substrate detects mismatches automatically and provides a controlled migration path.

## 2. The fingerprint format

```
ModelFingerprint = [u8; 16]
```

A 16-byte value derived from a canonical encoding of the model's identity.

## 3. Computation

```rust
fn compute_fingerprint(model_dir: &Path) -> ModelFingerprint {
    let mut hasher = blake3::Hasher::new();

    // 1. Model architecture
    let config = read_file(model_dir.join("config.json"));
    hasher.update(b"config.json:");
    hasher.update(&config);

    // 2. Tokenizer
    let tokenizer = read_file(model_dir.join("tokenizer.json"));
    hasher.update(b"tokenizer.json:");
    hasher.update(&tokenizer);

    // 3. Weights (hash of the safetensors file)
    let weights_hash = blake3_hash_file(model_dir.join("model.safetensors"));
    hasher.update(b"weights:");
    hasher.update(&weights_hash);

    // 4. Substrate-specific config
    hasher.update(b"vector_dim:");
    hasher.update(&384u32.to_le_bytes());
    hasher.update(b"normalize:");
    hasher.update(&[1u8]);  // We always normalize

    // Truncate to 16 bytes
    let full = hasher.finalize();
    full.as_bytes()[..16].try_into().unwrap()
}
```

Every component matters:

- **`config.json`** — defines model architecture (layers, hidden dim, etc.).
- **`tokenizer.json`** — defines tokenization.
- **Weights** — hashed to detect any tampering or version differences.
- **Substrate-specific config** — captures our normalization choice and dim.

Different models, different versions, or different tokenizer configs all produce different fingerprints.

## 4. The fingerprint is stable

For a given model directory, the fingerprint is deterministic. The same directory always produces the same fingerprint.

Across different machines, the same fingerprint means the same model. Operators can compare fingerprints across deployments to confirm they're using the same model without comparing weight bytes directly.

## 5. The fingerprint travels with vectors

Every memory's metadata carries its `embedding_model_fp`. This is set at encode time:

```rust
fn encode(text: &str) -> Memory {
    let vector = embedding_layer.embed(text).await?;
    Memory {
        vector,
        embedding_model_fp: embedding_layer.fingerprint(),
        // ... other fields
    }
}
```

Every query carries the current fingerprint:

```rust
fn recall(cue_text: &str) {
    let cue_vector = embedding_layer.embed(cue_text).await?;
    let current_fp = embedding_layer.fingerprint();

    // ... search the index
    // Filter results by fingerprint match
    let results = candidates.into_iter()
        .filter(|m| m.embedding_model_fp == current_fp)
        .collect();
}
```

## 6. Cross-fingerprint exclusion

When `RECALL` (or any vector-using operation) processes candidates, it filters out memories whose fingerprint doesn't match the current model's:

- Same fingerprint: included.
- Different fingerprint: excluded.

The exclusion is silent — the user sees only matching results. The reasoning: cross-model similarity scores are noise; returning them would degrade quality without warning.

For observability, the substrate tracks the fraction of excluded candidates per query. If the fraction is high, that's a signal of in-flight migration; the operator should be aware.

## 7. The migration window

During a model upgrade:

- All existing memories have the old fingerprint.
- New memories (encoded after the model swap) have the new fingerprint.
- Queries embedded with the new model match only new memories.

This means queries are partial during migration. To avoid this, operators should run `ADMIN_MIGRATE_EMBEDDINGS` before switching to query the new model. Detailed in [`08_migration.md`](08_migration.md).

## 8. The fingerprint table

The substrate maintains a per-shard table of fingerprints it has seen:

```
table: model_fingerprints
key: ModelFingerprint
value: { 
    model_name: String, 
    seen_at: u64, 
    memory_count_at_fingerprint: u64 
}
```

This table:

- Lets operators see the fingerprint history of a shard.
- Supports `ADMIN_STATS` queries about model migration progress.
- Helps diagnose "why are my queries returning fewer results than expected?".

## 9. Multiple models in flight

A shard may legitimately have memories with multiple fingerprints — during migration, before consolidation, in mixed deployments. The substrate supports this:

- Storage holds vectors with their fingerprints.
- Queries filter by the current fingerprint.
- The migration process re-embeds memories one by one, updating their fingerprint.

What's *not* supported: querying with a non-current fingerprint. Queries always use whatever model is currently configured. To query against an older model, you'd need to revert the configuration — which is a coarse operation.

## 10. Fingerprint comparison

```rust
impl ModelFingerprint {
    fn matches(&self, other: &ModelFingerprint) -> bool {
        // Constant-time comparison for safety
        constant_time_eq(self, other)
    }
}
```

Comparison is constant-time to avoid timing attacks (unlikely to matter for this use case but defensive).

## 11. The fingerprint as a model selector

In `ENCODE_VECTOR_DIRECT` (where the client provides a pre-computed vector), the client supplies a fingerprint. The substrate checks:

- The fingerprint is in its known set.
- The vector dimension matches what that fingerprint registered (e.g., a fingerprint for a 768-dim model can't carry a 384-dim vector).

If the fingerprint is unknown, the substrate rejects with `UnknownModel`. Operators can register additional fingerprints via `ADMIN_REGISTER_MODEL` (an admin-only opcode), specifying the model's vector dim and other properties.

## 12. The fingerprint and consolidation

When the consolidation worker creates a `Consolidated` memory, the new memory's fingerprint matches the current model's. Source memories may have older fingerprints; that's fine — consolidation reads the source vectors with their fingerprints and produces a new vector with the current fingerprint.

This is a small extra wrinkle in consolidation:

- If the source memories all have the current fingerprint, consolidation is straightforward.
- If some sources have old fingerprints, consolidation must either:
  - Skip them (loses some signal).
  - Re-embed them first (extra work).
- Default behavior: skip during partial-migration windows; once migration completes, all memories share the fingerprint and consolidation proceeds normally.

## 13. Truncation collision risk

The fingerprint is 16 bytes truncated from BLAKE3's full 32-byte output. Truncation collision probability:

- For 2 distinct models: 2^-128 (negligible).
- For 10,000 known models: ~10^-31 (still negligible).

The truncation is safe for our use case. If we ever need a stronger guarantee, the full 32-byte BLAKE3 hash is available; we'd just bump the storage format to carry 32 bytes.

## 14. The fingerprint vs the model name

The fingerprint is a *content-addressed* identifier. The model name (`"bge-small-en-v1.5"`) is a *human-readable* label.

These are separate:

- Two builds of the same model name might have different fingerprints (e.g., different tokenizer revisions).
- Two different model names might (in theory) share a fingerprint if they're byte-identical (won't happen in practice).

The substrate uses the fingerprint for correctness; the model name is for human convenience (logs, stats displays, configuration).

---

*Continue to [`08_migration.md`](08_migration.md) for the migration procedure.*
