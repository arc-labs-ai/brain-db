# 11.02 Decay Worker

The decay worker applies time-based salience decay to memories. It also handles access-boost (memories accessed recently get a salience bump).

## 1. Why decay

Salience represents how important / relevant a memory is. Without decay, every memory keeps its initial salience forever; the substrate can't distinguish "still useful" from "long-forgotten".

Decay over time:
- Episodic memories lose salience faster (default half-life: 30 days).
- Semantic memories lose salience slower (default half-life: 365 days).
- Consolidated memories: 90-day half-life.

Memories below a salience threshold are candidates for soft auto-FORGET (off by default; opt-in per agent).

## 2. The decay formula

For a memory with initial salience `s_0`, age `t` (in days), and half-life `h`:

```
salience(t) = s_0 × 2^(-t / h)
```

After one half-life, salience is `s_0 / 2`. After two, `s_0 / 4`. And so on.

Plus access boost: each access multiplies salience by `1 + boost`, capped at 1.0:

```
salience_after_access = min(1.0, salience × 1.1)    // 10% boost
```

The boost decays with the rest of the salience.

## 3. The cycle

Every hour (configurable), the decay worker:

1. Reads the memories table in batches.
2. For each memory, computes the new salience.
3. Writes back the updated salience.

The cycle is incremental: each cycle processes a batch (default 10,000 memories). Large shards take multiple cycles to process all memories; the worker resumes from where it left off.

## 4. The batch processing

```rust
async fn decay_cycle(state: &ShardState) -> Result<()> {
    let now = Timestamp::now();
    let mut last_processed = state.decay_cursor.load();

    let mut batch = Vec::new();
    let rtxn = state.metadata.begin_read()?;
    let memories = rtxn.open_table(MEMORIES)?;
    
    for entry in memories.range_after(&last_processed)? {
        if batch.len() >= 10_000 { break; }
        let (id, m) = entry?;
        let new_salience = compute_decay(&m, now);
        if new_salience != m.salience {
            batch.push((id, new_salience));
        }
    }
    drop(rtxn);

    if !batch.is_empty() {
        let mut wtxn = state.metadata.begin_write()?;
        let mut memories = wtxn.open_table(MEMORIES)?;
        for (id, new_salience) in &batch {
            let mut m = memories.get(id)?.unwrap();
            m.salience = *new_salience;
            memories.insert(id, &m)?;
        }
        wtxn.commit()?;
    }

    state.decay_cursor.store(batch.last().map(|(id, _)| *id).unwrap_or(last_processed));
    Ok(())
}
```

The cursor lets multiple cycles cover all memories.

## 5. The cursor

The decay worker tracks a cursor (the last MemoryId processed):

```
cursor at start: < first MemoryId.
cursor after one cycle: the last ID in the batch.
cursor at end of full pass: > last MemoryId; reset to < first.
```

A full pass takes (total memories / batch size) cycles. For 1M memories with batch=10K and interval=1h: 100 cycles, ~4 days for a full pass.

## 6. The "minor changes" optimization

If a memory's computed new salience is very close to its current salience (delta < 0.001), the worker skips the write:

```rust
if (new_salience - m.salience).abs() < 0.001 {
    continue;    // Not worth a write
}
```

This avoids many tiny writes that don't meaningfully change anything.

## 7. The access-boost worker

A separate worker (running every 10 seconds) handles access boosts:

```
1. Drain a buffer of recently-accessed MemoryIds.
2. For each, increment salience by 10% (capped at 1.0).
3. Write back in a single transaction.
```

The buffer is filled by request handlers (RECALL adds the returned memories' IDs).

## 8. The combination of decay and boost

A memory's final salience is:
- Initial salience (set at ENCODE).
- Modified by decay (continuous, applied each decay cycle).
- Modified by access boosts (applied each time the memory is returned in a RECALL).

Both workers can update the same memory. The decay worker reads-then-writes; if a boost happened in between, the decay worker may overwrite it. The two workers' cycles don't coordinate explicitly.

In practice:
- Boost cycle is faster (10s vs 1h).
- Boosts happen per access; decay applies less frequently.
- Net effect: boosts are visible quickly, then slowly decay.

If a memory is boosted just before a decay cycle, the boost is captured. If it's boosted after a decay cycle's read, the boost is overwritten. A subsequent boost cycle re-applies it.

This is acceptable: salience isn't precise; small inaccuracies are fine.

## 9. The auto-forget option

If `agent.auto_forget_below_salience > 0` is configured, memories with salience below the threshold are soft-FORGOTTEN automatically.

The decay worker, when it computes a sub-threshold salience, can issue a soft FORGET on that memory.

This is **off by default**. Auto-forgetting is a strong default; many users want full control over deletion.

For agents that opt in, the threshold is typically 0.05 or so — only very-low-salience memories get auto-forgotten.

## 10. Decay constants

The half-lives are configurable per kind:

```toml
[memory.decay]
episodic_half_life = "30d"
semantic_half_life = "365d"
consolidated_half_life = "90d"
boost_factor = 0.10                 # 10% per access
```

Different applications may want different rates. A chat assistant may want fast decay (forget conversations quickly); a knowledge base may want very slow decay.

## 11. The "no decay" option

For workloads that don't want decay, set the half-life to a very large value (e.g., 100 years). Effectively no decay.

Or disable the worker entirely:

```toml
[workers.decay]
enabled = false
```

The substrate continues to work; salience just doesn't change over time.

## 12. The cost of decay

Per cycle (10K memories):
- Read: ~50 ms (batch range scan).
- Compute: ~10 ms.
- Write: ~50-100 ms.
- Total: ~150 ms.

Per-memory cost: ~15 µs. Negligible relative to other operations.

## 13. The worker as a "soft real-time" task

The decay worker is soft-real-time: missing a cycle is fine. Missing several cycles delays decay slightly but doesn't cause incorrectness.

Operators can tolerate the worker being temporarily paused (e.g., during heavy write load).

## 14. Long-term memories

For very old memories, decay has compounded. A memory with initial salience 1.0 and 1-year-old:

- Episodic (30-day half-life): salience ≈ 0.0009.
- Semantic (365-day half-life): salience ≈ 0.5.
- Consolidated (90-day half-life): salience ≈ 0.06.

This is by design — episodic memories should fade fast unless reinforced via accesses.

For applications that want all memories to remain accessible regardless of age, configure semantic-only or disable decay.

## 15. The decay vs RECALL interaction

RECALL doesn't filter by salience by default — even low-salience memories are returned if their similarity is high.

Agents can filter by salience explicitly:

```
recall.filter.min_salience = Some(0.1)
```

This excludes memories below the threshold.

## 16. The salience boost in RECALL

When a memory is returned in a RECALL response, it's added to the access-boost buffer. The next cycle of the access-boost worker applies the boost.

So accessed memories slowly become more salient. Unaccessed memories slowly become less salient. This is the "use it or lose it" pattern.

## 17. Rich-get-richer effects

Highly-salient memories are more likely to be returned (if salience-filtering is used) → more accesses → more boosts → higher salience. This positive feedback can entrench certain memories.

For most workloads, this is fine. For applications wanting to balance, periodically re-rank or use lower boost factors.

---

*Continue to [`03_consolidation.md`](03_consolidation.md) for consolidation.*
