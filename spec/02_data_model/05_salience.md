# 02.05 Salience

**Salience** is a single number in [0, 1] representing how important a memory is. It drives ranking in `RECALL`, eligibility for consolidation and eviction, and rate of decay. This file specifies the model: how salience is initially computed, how it updates on access, how it decays, and how it's normalized.

## 1. The role of salience

Salience answers: "of two memories that match this query equally well by similarity, which is the more important?".

Without a salience signal, ranking is purely similarity-based, which is a thin signal. A user's casual mention of a date five years ago has the same similarity to "do you remember that date?" as their wedding date — but the wedding date should rank higher.

Salience captures that "should rank higher". It accumulates from multiple inputs (explicit hints, surprise, access patterns, kind, age) and acts as the ranking tiebreaker (and partial ordering signal even when similarity is close but not equal).

## 2. The conceptual model

Salience for a memory M is a function of:

- **Initial salience** at encode time: `S_0(M)`.
- **Cumulative access boost** since encode: `B(M)`.
- **Cumulative decay** since encode: `D(M)`.

Combined as:

```
salience(M) = clip( S_0(M) + B(M) - D(M),  0, 1 )
```

The pieces are detailed below.

## 3. Initial salience

When a memory is encoded, its initial salience `S_0(M)` is computed as:

```
S_0(M) = clip( w_hint * H + w_surprise * U + w_kind * K + S_base,  0, 1 )
```

Where:

- `H` is the agent-supplied salience hint, in [-1, +1]. Default 0.
- `U` is the surprise score: distance from the centroid of recently-encoded memories in the same context. Higher distance = more surprising. Normalized to [0, 1].
- `K` is the kind weight: 0.5 for `Episodic`, 0.7 for `Semantic`, 0.6 for `Consolidated` (consolidated memories start somewhat above episodic but below semantic — they represent learned patterns but aren't core knowledge).
- `S_base` is a baseline (default 0.4) — every memory starts at least somewhat salient.
- The weights `w_hint`, `w_surprise`, `w_kind` are configuration-tunable; defaults are 0.4, 0.2, 0.4.

In practice, with default weights and zero hint:

- A typical episodic memory starts at salience ≈ 0.6.
- A semantic memory starts at salience ≈ 0.7.
- An agent-flagged "important" episodic memory (`H = 1`) starts at salience ≈ 0.8 — 1.0.

The initial salience is bounded to [0, 1]; clipping happens at the end of the formula.

### 3.1 Surprise calculation

The surprise score U is computed as:

```
U = clip( 1 - cos_sim(vector(M), centroid_recent(context)),  0, 1 )
```

Where `centroid_recent(context)` is the mean of the last K vectors encoded in the same context (K = 100 by default; configurable).

A memory whose vector is far from the recent centroid is "surprising" and gets a higher U; a memory that's similar to many recent ones is "expected" and gets a lower U.

This serves as a cheap novelty signal: if the agent encodes something like its existing memories, the system doesn't get excited; if it encodes something genuinely new, the system flags it as more salient.

### 3.2 Kind-aware base salience

The `K` term reflects different baseline importance for different kinds:

- **Episodic** — events happen all the time; baseline is moderate (0.5).
- **Semantic** — stable knowledge is by nature more important than any individual event (0.7).
- **Consolidated** — derived patterns sit between (0.6).

These constants can be tuned at deployment time but the relative ordering is fixed.

## 4. Access-based boost

Each time a memory is accessed (read by `RECALL`, `PLAN`, or `REASON`), its salience is boosted:

```
B_one_access = (1 - salience) * boost_rate
```

Where `boost_rate` is small (default 0.05).

The shape: a low-salience memory gets a larger boost per access than a high-salience one. This is sigmoid-like — salience saturates as it approaches 1, so re-accessing a high-salience memory adds less than re-accessing a low-salience one.

The full boost over a memory's lifetime is the cumulative sum of per-access boosts, which approaches a limit asymptotically as the salience approaches 1.

### 4.1 Why boost on read

The reasoning: memories that get recalled often are useful; they should rank higher in future queries; making them harder to forget over time.

This is *use-it-or-lose-it* dynamics: memories that prove their relevance gain salience, while memories that are never queried decay.

### 4.2 Asynchrony

Access boosts are applied asynchronously. A `RECALL` returns its result immediately; the salience update is enqueued and applied by the writer task without blocking the response.

This is a real engineering trade-off. Synchronous updates would tie every read to a write, killing the read path's lock-freedom. Async updates mean a brief window where the read happened but the salience hasn't yet updated; we accept it.

The async lag is typically <10 ms in normal operation, much less than the time between successive reads of the same memory.

## 5. Decay

Salience decays exponentially over time. The functional form, drawing from the [Ebbinghaus forgetting curve](https://en.wikipedia.org/wiki/Forgetting_curve):

```
salience_after_decay(M, t) = salience(M) * exp( -decay_rate(M) * (t - t_last_decay(M)) )
```

Where:

- `t` is the current time.
- `t_last_decay(M)` is the last time decay was applied to M.
- `decay_rate(M)` is the per-memory decay rate.

### 5.1 Per-memory decay rate

The decay rate is **kind-dependent**:

| Kind | Half-life (default) |
|---|---|
| Episodic | 30 days |
| Semantic | 365 days |
| Consolidated | 90 days |

Half-life is the time for salience to fall to half its current value. Equivalently:

```
decay_rate = ln(2) / half_life
```

These defaults match a rough cognitive intuition — recent events fade quickly, durable knowledge fades slowly, learned patterns somewhere in between. They are configuration-tunable per deployment.

### 5.2 When decay is applied

Decay is applied:

- **Lazily on read.** When a memory is loaded for a `RECALL`, its salience is updated by the elapsed time since last update. The updated value is returned and persisted.
- **Periodically by the decay worker.** A background sweep applies decay to memories that haven't been read for a long time. See [11. Background Workers](../11_background_workers/) §Decay.

### 5.3 The decay floor

Decay never lowers salience below `salience_floor` (default: 0.05). Memories at the floor stay there indefinitely; they're never automatically erased by decay alone.

Memories below an `eviction_threshold` (default: 0.1, but distinct from the floor) become *eligible* for eviction by the consolidation worker — but they stay until eviction actually happens.

## 6. Recency boost

Beyond access-based and decay-based dynamics, recently-created memories get a one-time recency boost:

```
recency_boost(M, t) = recency_strength * exp( -(t - created_at(M)) / recency_window )
```

Where:

- `recency_strength` default: 0.2.
- `recency_window` default: 7 days.

The effect: a memory created an hour ago gets a recency boost of ~0.2; a memory created a day ago gets ~0.17; a week-old memory gets ~0.07; older memories get effectively zero.

Recency boost is **applied at query time, not stored.** It influences ranking in `RECALL` but doesn't permanently change salience. This avoids the bookkeeping of constantly updating recency boosts.

The combined effective salience for ranking:

```
effective_salience(M, t) = salience(M) + recency_boost(M, t)
```

`effective_salience` is used in ranking; the persisted `salience` is what's stored.

## 7. Confidence vs salience

A common confusion: confidence and salience are different things.

- **Salience** is intrinsic to the memory: how important is M, regardless of any specific query?
- **Confidence** is per-query: given this query, how confident are we that M is the right match?

Confidence is computed at query time from the similarity score and salience, calibrated against benchmark distributions. See [09. Cognitive Operations](../09_cognitive_operations/) §RECALL for the full computation.

A high-salience memory may have low confidence for a specific query (it's important but unrelated); a low-salience memory may have high confidence (it's clearly the most relevant of its peers).

## 8. The boundary cases

### 8.1 Salience clamping

If the formula produces a value > 1, it's clamped to 1. If < 0 (unusual but possible with negative hint), clamped to 0.

### 8.2 Salience persistence

Persistent salience is updated on:

- Encode (initial value).
- Access (boost; via async update).
- Decay sweep (background; lazy on read).
- Consolidation (newly-created consolidated memories get fresh initial salience).

Persistent salience is NOT updated on:

- Recency boost (applied at query time, not stored).
- Failed access (e.g., a `RECALL` that returns no results; no memory was actually accessed).

### 8.3 Concurrent access

If two `RECALL` operations both access the same memory in parallel, both salience boosts are applied (atomically, via the writer queue). The order of application doesn't matter — the formula is order-independent in expectation.

### 8.4 The decay edge case

If decay hasn't been applied to a memory in a very long time (e.g., the agent went silent for a year), the lazy decay-on-read produces a large drop. This is correct behavior, not a bug.

## 9. Tuning

The salience formula has many constants. Each is configurable per deployment, but the defaults are calibrated to give reasonable behavior out-of-the-box.

Recommended tuning approach:

1. **Don't tune** unless you have a specific complaint about ranking behavior.
2. If memories from too long ago are over-ranked, increase decay rates (shorter half-lives).
3. If memories are forgotten too quickly, decrease decay rates (longer half-lives).
4. If high-salience hint isn't strong enough, increase `w_hint`.
5. If new memories take too long to "settle in", increase `recency_strength`.

The constants are documented in [14. Observability + Operations](../14_observability_ops/) §Configuration.

## 10. Summary

Salience is a single number in [0, 1] per memory, computed by combining initial encode-time inputs, accumulating boosts on access, and decaying over time at a kind-dependent rate. A short-term recency boost is layered on at query time. The result is a value that captures "how important is this memory, considered on its own merits", available for ranking and for triggering background eviction when memories fall too far.

---

*Continue to [`06_edges.md`](06_edges.md) for the edge model.*
