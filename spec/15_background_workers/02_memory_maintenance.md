# 15.02 Memory Maintenance Workers

The three workers that keep memory salience, consolidation, and the HNSW index healthy: decay, consolidation, and HNSW maintenance.

## Decay Worker

The decay worker applies time-based salience decay to memories. It also handles access-boost (memories accessed recently get a salience bump).

### 1. Why decay

Salience represents how important / relevant a memory is. Without decay, every memory keeps its initial salience forever; the system can't distinguish "still useful" from "long-forgotten".

Decay over time:
- Episodic memories lose salience faster (default half-life: 30 days).
- Semantic memories lose salience slower (default half-life: 365 days).
- Consolidated memories: 90-day half-life.

Memories below a salience threshold are candidates for soft auto-FORGET (off by default; opt-in per space).

### 2. The decay formula

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

### 3. The cycle

Every hour (configurable), the decay worker:

1. Reads the memories table in batches.
2. For each memory, computes the new salience.
3. Writes back the updated salience.

The cycle is incremental: each cycle processes a batch (default 10,000 memories). Large shards take multiple cycles to process all memories; the worker resumes from where it left off.

### 4. The batch processing

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

### 5. The cursor

The decay worker tracks a cursor (the last MemoryId processed):

```
cursor at start: < first MemoryId.
cursor after one cycle: the last ID in the batch.
cursor at end of full pass: > last MemoryId; reset to < first.
```

A full pass takes (total memories / batch size) cycles. For 1M memories with batch=10K and interval=1h: 100 cycles, ~4 days for a full pass.

### 6. The "minor changes" optimization

If a memory's computed new salience is very close to its current salience (delta < 0.001), the worker skips the write:

```rust
if (new_salience - m.salience).abs() < 0.001 {
    continue;    // Not worth a write
}
```

This avoids many tiny writes that don't meaningfully change anything.

### 7. The access-boost worker

A separate worker (running every 10 seconds) handles access boosts:

```
1. Drain a buffer of recently-accessed MemoryIds.
2. For each, increment salience by 10% (capped at 1.0).
3. Write back in a single transaction.
```

The buffer is filled by request handlers (RECALL adds the returned memories' IDs).

### 8. The combination of decay and boost

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

### 9. The auto-forget option

If `agent.auto_forget_below_salience > 0` is configured, memories with salience below the threshold are soft-FORGOTTEN automatically.

The decay worker, when it computes a sub-threshold salience, can issue a soft FORGET on that memory.

This is **off by default**. Auto-forgetting is a strong default; many users want full control over deletion.

For agents that opt in, the threshold is typically 0.05 or so — only very-low-salience memories get auto-forgotten.

### 10. Decay constants

The half-lives are configurable per kind:

```toml
[memory.decay]
episodic_half_life = "30d"
semantic_half_life = "365d"
consolidated_half_life = "90d"
boost_factor = 0.10                 # 10% per access
```

Different applications may want different rates. A chat assistant may want fast decay (forget conversations quickly); a knowledge base may want very slow decay.

### 11. The "no decay" option

For workloads that don't want decay, set the half-life to a very large value (e.g., 100 years). Effectively no decay.

Or disable the worker entirely:

```toml
[workers.decay]
enabled = false
```

The system continues to work; salience just doesn't change over time.

### 12. The cost of decay

Per cycle (10K memories):
- Read: ~50 ms (batch range scan).
- Compute: ~10 ms.
- Write: ~50-100 ms.
- Total: ~150 ms.

Per-memory cost: ~15 µs. Negligible relative to other operations.

### 13. The worker as a "soft real-time" task

The decay worker is soft-real-time: missing a cycle is fine. Missing several cycles delays decay slightly but doesn't cause incorrectness.

Operators can tolerate the worker being temporarily paused (e.g., during heavy write load).

### 14. Long-term memories

For very old memories, decay has compounded. A memory with initial salience 1.0 and 1-year-old:

- Episodic (30-day half-life): salience ≈ 0.0009.
- Semantic (365-day half-life): salience ≈ 0.5.
- Consolidated (90-day half-life): salience ≈ 0.06.

This is by design — episodic memories should fade fast unless reinforced via accesses.

For applications that want all memories to remain accessible regardless of age, configure semantic-only or disable decay.

### 15. The decay vs RECALL interaction

RECALL doesn't filter by salience by default — even low-salience memories are returned if their similarity is high.

Agents can filter by salience explicitly:

```
recall.filter.min_salience = Some(0.1)
```

This excludes memories below the threshold.

### 16. The salience boost in RECALL

When a memory is returned in a RECALL response, it's added to the access-boost buffer. The next cycle of the access-boost worker applies the boost.

So accessed memories slowly become more salient. Unaccessed memories slowly become less salient. This is the "use it or lose it" pattern.

### 17. Rich-get-richer effects

Highly-salient memories are more likely to be returned (if salience-filtering is used) → more accesses → more boosts → higher salience. This positive feedback can entrench certain memories.

For most workloads, this is fine. For applications wanting to balance, periodically re-rank or use lower boost factors.

## Consolidation Worker

The consolidation worker promotes groups of related Episodic memories into Consolidated memories — summaries that distill the essence of the group.

### 18. The cognitive metaphor

Human memory has a similar process: short-term experiences (episodic) are gradually consolidated into long-term knowledge (semantic). The implementation doesn't claim to model this faithfully — but the abstraction is useful.

In the system:
- Many small Episodic memories accumulate.
- Periodically, clusters of related memories are identified.
- A summary (a Consolidated memory) is generated that captures the cluster.
- The original Episodic memories remain (linked to the consolidated via DERIVED_FROM edges).

### 19. Why consolidate

Without consolidation:
- The space's memory is a flat collection of episodic events.
- RECALL returns relevant episodes one at a time.
- The agent has to re-aggregate insights itself.

With consolidation:
- Higher-level summaries are first-class memories.
- RECALL can return the consolidated memory plus its sources.
- The agent gets pre-aggregated session.

### 20. The cycle

Every 5 minutes (configurable):

1. The worker iterates per-session.
2. For each session, find clusters of memories that warrant consolidation.
3. For each cluster, generate a summary (via an LLM call, see "The summarization" below).
4. Encode the summary as a Consolidated memory.
5. Add DERIVED_FROM edges to source memories.

### 21. Cluster identification

A cluster is a group of memories that:

- Share a session.
- Are episodic.
- Are temporally close (e.g., within a 24-hour window).
- Are similar in vector space (cosine similarity > 0.6 within the cluster).

Algorithms:

- Pull recent episodic memories from the session.
- Group by vector similarity using a simple density-based clustering (DBSCAN-style).
- A cluster needs at least 5 memories (configurable threshold).

The clustering runs on the metadata of memories (no need to load text yet).

### 22. The threshold trigger

The consolidation worker also has a threshold trigger:

- When a session exceeds 50 episodic memories (configurable), schedule consolidation immediately rather than waiting for the next cycle.
- This prevents large sessions from being unrepresented in semantic memory.

### 23. The summarization

Generating the summary requires an LLM. Brain doesn't ship with an LLM; it integrates with an external service:

```rust
async fn summarize(memories: &[MemoryText]) -> Result<String> {
    let prompt = build_consolidation_prompt(memories);
    let response = llm_service.generate(prompt).await?;
    Ok(response.text)
}
```

Configuration specifies the broker, not a credential: the summarizer resolves per namespace like every other LLM consumer, so a consolidation run is paid for by the tenant whose memories it consolidates. Llama, GPT, or any specific model are not bundled — the integration is pluggable.

For deployments without an LLM, consolidation is disabled. The system works fine without it; clients just don't get Consolidated memories.

### 24. The summarization prompt

The default prompt:

```
You are a memory consolidation system. Below are several memories from the same session.
Summarize them into a single, concise paragraph that captures the key information.

Memories:
1. {memory_1.text}
2. {memory_2.text}
...

Summary:
```

The prompt is configurable. Operators can tune it for their use case.

### 25. The encoded consolidated memory

After summarization:

1. The summary text is encoded as a new memory (kind = Consolidated).
2. DERIVED_FROM edges are created from the new memory to each source memory.
3. The source memories' metadata is updated with `consolidated_into = new_memory_id`.

This is a single transactional unit: the new memory, its edges, and the metadata updates all commit atomically.

### 26. The Consolidated memory's properties

A Consolidated memory:

- Has its own MemoryId.
- Has its own vector (from embedding the summary text).
- Has DERIVED_FROM edges to all source memories.
- Has a 90-day half-life (slower decay than Episodic).
- Is searchable like any other memory.

Sources are still searchable too — RECALL might return the Consolidated and the Episodics. The agent can then choose what to use.

### 27. The cost

Per cluster:
- Cluster identification: ~10-50 ms.
- Summarization LLM call: 500-2000 ms (network + LLM latency).
- Encoding the summary: ~5-10 ms.
- Edge creation: ~5 ms.

Total per cluster: ~1-2 seconds dominated by the LLM call.

For a typical session's growth rate, consolidation runs occasionally (every few minutes) and produces a few new Consolidated memories per cycle.

### 28. The "skip" cases

Consolidation skips clusters that:

- Have already been consolidated (memories link to an existing Consolidated).
- Have very high similarity (a near-duplicate; unlikely to add value).
- Have very low coherence (vectors are scattered; not a real cluster).

Skipped clusters may be revisited in future cycles if the membership changes.

### 29. The "consolidation of consolidations"

A Consolidated memory is a memory like any other. It can itself be part of a cluster of related Consolidated memories. The worker can recursively consolidate.

In practice, this is rare — the second-order summaries are at a high abstraction level. Recursive consolidation is capped at 2-3 levels.

### 30. The "session boundary"

Consolidation respects session boundaries:
- Episodic memories from one session don't get consolidated with those from another.
- Each session's consolidation is independent.

This matches the agent-data-model expectation: sessions are bounded scopes.

### 31. The "active" filter

Only active (non-tombstoned) memories are consolidated. Tombstoned memories are skipped — they're going away anyway.

If a source memory is FORGOTTEN after its Consolidated derivation:
- The DERIVED_FROM edge points to a tombstoned source.
- The Consolidated memory still exists.
- Eventually, the maintenance worker cleans up the orphaned edge.

The Consolidated memory's text doesn't change — it summarizes what was true at consolidation time.

### 32. The "approval" workflow option

Some applications want human approval of consolidations before they're stored. A "draft" mode supports this:

- The summary is generated and stored as a draft (not yet a memory).
- A client (a UI, presumably) reviews and approves.
- On approval, the system encodes the memory.

This is an opt-in mode, configured per-session. By default, consolidation is fully automatic.

### 33. The disabled state

If the LLM service is unavailable or consolidation is disabled, the worker becomes a no-op:

```toml
[workers.consolidation]
enabled = false
```

The system works fine. Just no Consolidated memories.

### 34. The consolidation latency vs freshness trade-off

- More frequent consolidation: more LLM calls, fresher summaries.
- Less frequent: fewer LLM calls, staler summaries (memories that should be consolidated wait).

Operators tune the interval based on:
- LLM cost.
- Workload write rate.
- Acceptable consolidation lag.

### 35. The "quality" question

Auto-generated summary quality depends on the LLM. Brain doesn't validate quality — that's the LLM's job.

If summaries are poor (bad LLM, bad prompt), consolidation can produce noise. Operators may disable consolidation until the quality is acceptable.

For high-quality summaries, the value-add is significant: the agent gets pre-aggregated session.

## HNSW Maintenance Worker

The index maintenance worker monitors index quality and rebuilds when needed. The mechanism is described in [09.03 HNSW Lifecycle](../09_indexing/03_hnsw_lifecycle.md); this section describes the worker side.

### 36. The cycle

Every 5 minutes, the worker:

1. Reads per-shard index statistics.
2. Estimates current recall via sampled queries.
3. Decides on action:
   - None: no work needed.
   - Schedule rebuild: if degradation is mild.
   - Immediate rebuild: if severe.
4. If rebuilding, runs the rebuild process.

### 37. Decision criteria (recap)

```rust
fn decide_action(stats: IndexStats) -> Action {
    if stats.tombstone_ratio > 0.30 {
        return Action::FullRebuild;
    }
    if stats.recall_estimate < 0.90 {
        return Action::FullRebuild;
    }
    if stats.tombstone_ratio > 0.15 || stats.recall_estimate < 0.93 {
        return Action::ScheduleRebuildSoon;
    }
    Action::None
}
```

The thresholds are configurable. Defaults are conservative — most shards never trigger a rebuild.

### 38. The recall estimation

For sampled recent queries:

```rust
async fn estimate_recall(state: &ShardState) -> f32 {
    let samples = state.recent_query_samples.read(50);
    let mut overlap_sum = 0.0;
    
    for sample in samples {
        let baseline_results = sample.original_results;        // Top K with normal ef
        let truth_results = state.hnsw.search(
            &sample.query,
            sample.k,
            ef = 500,                                          // Much larger ef
        ).await?;
        
        let overlap = compute_overlap(&baseline_results, &truth_results);
        overlap_sum += overlap;
    }
    
    overlap_sum / samples.len() as f32
}
```

The estimate is based on a small sample (50 queries). It's noisy but gives a useful signal.

### 39. The rebuild process

When a rebuild is decided:

```rust
async fn rebuild(state: &ShardState) -> Result<()> {
    let snapshot_lsn = state.current_lsn();
    let new_index = HnswIndex::new(M=16, ef_construction=200);
    
    // Iterate active memories from metadata
    let rtxn = state.metadata.begin_read()?;
    let memories = rtxn.open_table(MEMORIES)?;
    let mut count = 0;
    
    for entry in memories.iter()? {
        let (id, m) = entry?;
        if !m.is_active() { continue; }
        
        let vector = state.arena.read_vector(m.slot_id);
        new_index.insert(id, vector);
        count += 1;
        
        if count % 500 == 0 {
            glommio::yield_now().await;
        }
    }
    
    // Catch up to current LSN
    let pending_inserts = state.wal.records_since(snapshot_lsn).await?;
    for record in pending_inserts {
        if let WalRecord::Encode(e) = record {
            new_index.insert(e.memory_id, e.vector);
        }
    }
    
    // Atomic swap
    state.hnsw.swap(Arc::new(new_index));
    
    Ok(())
}
```

The rebuild reads from the metadata, builds a fresh HNSW, then atomically swaps it in.

### 40. The atomic swap

The swap is a single ArcSwap operation. After the swap:
- New queries see the rebuilt index.
- In-flight queries (using the old index) continue and complete.
- The old index is freed when no readers reference it.

### 41. Memory during rebuild

During rebuild, two HNSW indexes are in memory:
- The active one (still serving queries).
- The new one being built.

For a 1M-memory index: ~300 MB peak usage during rebuild. For 10M: ~3 GB.

A configuration `ann.rebuild_max_memory_gb` bounds this. If a rebuild would exceed the limit, it's aborted (with a warning).

### 42. The catch-up phase

Between starting the rebuild and finishing, new encodes happen. These are missed by the rebuild's read-from-metadata snapshot.

The catch-up phase replays WAL records from `snapshot_lsn` to current. It applies any encodes that happened during the build.

The catch-up is fast (typically <1 second) because the rebuild itself is the long part (10s of seconds at scale).

### 43. The "swap" timing

The swap is a single atomic operation:

```rust
state.hnsw.store(Arc::new(new_index));
```

After this, all new queries use the new index. The pre-swap state is captured by readers' Arc references; their queries complete on the old index, then drop it.

The swap moment is microseconds. Tail latency around the swap may have a brief spike (Arc deallocation), but typically negligible.

### 44. Rebuild duration

For typical workloads:

| Memory count | Rebuild duration |
|---|---|
| 100K | ~1 sec |
| 1M | ~10 sec |
| 10M | ~2 min |
| 100M | ~30 min |

The rebuild is parallel within the build phase (using multiple Glommio tasks for inserts).

### 45. The "rebuild while running" guarantee

During rebuild:
- Reads continue at full performance against the old index.
- Writes continue against the old index (they're applied to it, then queued for the catch-up of the new one).
- The new index is built in the background.

Request latencies aren't affected (other than the brief spike at swap time and the memory pressure during rebuild).

### 46. The cost

A rebuild costs:
- CPU: ~10-30 sec at full speed for 1M memories.
- Memory: ~150 MB additional (the new index).
- Disk I/O: ~2 GB read (vectors from arena).

This is significant but bounded. A shard rebuilds rarely (typically every few weeks under normal workloads).

### 47. Monitoring rebuild progress

Per-rebuild metrics:

- `hnsw_rebuild_in_progress`: 0 or 1.
- `hnsw_rebuild_progress_pct`: 0–100.
- `hnsw_rebuild_estimated_remaining_sec`: estimate.
- `hnsw_rebuild_total_count`: counter, increments on each rebuild.
- `hnsw_rebuild_last_duration_sec`: how long the last one took.

Operators monitor these to track maintenance health.

### 48. The "manual rebuild" override

`ADMIN_REBUILD_ANN <shard_id>` triggers an immediate rebuild, bypassing the threshold-based scheduling. Use cases:

- After a known degradation event (mass deletion).
- Before a benchmark.
- For debugging.

The operation is async; returns immediately, rebuild runs in the background.

### 49. The "no rebuild" option

If rebuild is disabled (workers.hnsw_maintenance.enabled = false):
- The HNSW degrades over time (more tombstones, drift).
- Recall slowly drops.
- Eventually, manual intervention is needed.

For deployments that operate in narrow windows where rebuild can't run, this option exists. Most deployments leave it enabled.

### 50. The "rebuild backlog"

If the workload generates tombstones faster than rebuilds can remove them, warnings are logged. The cycle of "rebuild → fill with tombstones → trigger another rebuild" can dominate background work.

Operators address this by:
- Reducing tombstone generation rate (less FORGET).
- Increasing parallelism on rebuilds.
- Splitting the shard.

### 51. The interaction with snapshots

When a snapshot is taken (`ADMIN_SNAPSHOT_CREATE`), the snapshot includes the current HNSW state (if persistence is enabled).

The maintenance worker shouldn't run concurrent with snapshot creation. The two are serialized: if a snapshot is in progress, the worker waits; if the worker is running, the snapshot waits briefly.

### 52. The post-rebuild verification

After a rebuild, the worker verifies:
- The new index has the same node count as the metadata's active memory count.
- Sampled queries against the new index return reasonable results.

If verification fails, an alert is logged and (in some configurations) the system reverts to the old index. Such failures are bugs; reverts are a safety net.

---

*Continue to [`03_substrate_sweepers.md`](03_substrate_sweepers.md) for the substrate cleanup workers.*
