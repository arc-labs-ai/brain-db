# 02.07 Memory Kinds

A memory has a **kind**: one of `Episodic`, `Semantic`, or `Consolidated`. The kind influences how the memory is treated by salience, decay, ranking, and the consolidation worker.

The trichotomy comes from the cognitive-science distinction between [episodic and semantic memory](https://en.wikipedia.org/wiki/Explicit_memory) (Tulving 1972). Brain adds a third kind, `Consolidated`, to represent memories explicitly produced by the consolidation worker — distinct from both raw episodes and pure abstract knowledge.

## 1. The three kinds

```rust
enum MemoryKind {
    Episodic = 0,
    Semantic = 1,
    Consolidated = 2,
}
```

Numeric values are stable; persistence uses the integer encoding.

## 2. Episodic

A specific event, observation, or experience. Tied to time and (often) place.

### 2.1 Examples

- "User said 'I prefer dark mode' at 14:32 today."
- "Email from Alice arrived at 09:15 mentioning the budget."
- "I (the agent) decided to break up the task into three steps after seeing the failed attempt."
- "The deployment script returned exit code 137."

Each is a single, time-bound observation.

### 2.2 Default

`Episodic` is the **default** kind for `ENCODE`. Most agent observations are episodic; the agent is observing a stream of events and recording them.

### 2.3 Salience treatment

- **Initial salience** — kind weight 0.5 (moderate).
- **Decay** — half-life 30 days (faster). Episodic memories fade as they age.
- **Eviction eligibility** — episodic memories are the primary candidates for eviction once salience falls below threshold.

### 2.4 Consolidation candidate

Episodic memories are the *input* to consolidation. The consolidation worker scans episodic memories and clusters related ones into `Consolidated` memories.

After consolidation, the original episodic memories may be:

- **Retained** (their salience was high enough that they're kept).
- **Evicted** (their salience was below threshold and consolidation captured the essence).

The decision is salience-driven; consolidation doesn't automatically delete its sources.

## 3. Semantic

Stable knowledge that's not tied to a specific event.

### 3.1 Examples

- "The user prefers dark mode." (a stable fact, not the specific moment of stating it)
- "Budget approval requires CFO sign-off." (a rule, not a specific approval event)
- "Python uses reference counting plus a generational GC." (general knowledge)

Each is an abstract claim, not tied to a specific observation moment.

### 3.2 Creation

Semantic memories are created in two ways:

- **Explicit by the agent.** The agent encodes a memory with `kind = Semantic`, marking it as a stable claim.
- **Promotion from Consolidated.** A consolidated memory may be promoted to semantic when its salience accumulates enough — i.e., the substrate observes that the pattern is robust and durable.

There is **no automatic episodic-to-semantic promotion** in v1. Promotion requires either the agent's explicit choice or the consolidation pipeline.

### 3.3 Salience treatment

- **Initial salience** — kind weight 0.7 (higher).
- **Decay** — half-life 365 days (much slower). Semantic memories fade slowly.
- **Eviction eligibility** — semantic memories are the *last* to be evicted. The substrate is reluctant to let go of stable knowledge.

### 3.4 Consolidation behavior

Semantic memories are *not* consolidation candidates as input. They are the abstraction layer above what consolidation produces.

## 4. Consolidated

A summary or pattern derived from multiple episodic memories by the consolidation worker.

### 4.1 Examples

- "User prefers dark interfaces" (derived from many episodic observations of dark-mode-related interactions).
- "Deployments often fail on Friday afternoons" (derived from a pattern in deployment-event memories).
- "The customer support team escalates X-class issues to engineering" (derived from observations of escalation events).

Each captures a pattern the substrate noticed across multiple episodic memories.

### 4.2 Creation

`Consolidated` memories are created exclusively by the consolidation worker. Agents do **not** explicitly create consolidated memories — the agent encoding a summary directly should mark it as `Semantic`, not `Consolidated`. The `Consolidated` kind is reserved for the substrate's automated outputs.

Each `Consolidated` memory has `DERIVED_FROM` edges pointing to its source episodic memories, providing provenance.

### 4.3 Salience treatment

- **Initial salience** — kind weight 0.6 (between episodic and semantic).
- **Decay** — half-life 90 days (between episodic and semantic).
- **Eviction eligibility** — moderate. Consolidated memories age out slower than episodic but faster than semantic.

### 4.4 Promotion

If a consolidated memory's salience climbs (e.g., it's frequently accessed and confirmed by additional observations), it becomes a candidate for promotion to `Semantic`. The promotion threshold is configurable (default: salience ≥ 0.85 sustained for ≥ 30 days).

Promotion changes the kind, recomputes the decay, and clears most salience boost (resets to a baseline). The memory's content is unchanged.

## 5. Kind transitions

Allowed transitions:

| From | To | Trigger |
|---|---|---|
| Episodic | Semantic | Agent explicit (via `ADMIN_RECLASSIFY` or similar) |
| Episodic | (consolidated as DERIVED_FROM) | Consolidation worker; the episodic memory itself stays episodic; a new Consolidated memory is created |
| Consolidated | Semantic | Promotion based on salience and confirmation |
| Semantic | Episodic | Not allowed |
| Semantic | Consolidated | Not allowed |
| Consolidated | Episodic | Not allowed |

The disallowed transitions exist because they don't make sense — semantic knowledge degrading to a single episodic event is not a coherent operation; consolidated patterns becoming events is similarly nonsense.

## 6. Effect on cognitive operations

### 6.1 RECALL ranking

Different kinds rank differently when other factors are equal:

- For "What did the user say?" — episodic memories rank higher (recent specific events).
- For "What does the user prefer?" — semantic memories rank higher (stable knowledge).
- For "What patterns has the user shown?" — consolidated memories rank higher.

The substrate doesn't auto-detect the question type; the agent's filter parameter (or the planner's heuristics) selects which kind(s) to weight.

### 6.2 PLAN

Planning generally prefers semantic and consolidated memories as world-model facts; episodic memories are evidence but typically not the planning state.

### 6.3 REASON

Reasoning uses all three kinds:

- Episodic memories as evidence ("at time T, the user said X").
- Semantic memories as rules ("rule X applies in case Y").
- Consolidated memories as patterns ("similar situations have these outcomes").

### 6.4 FORGET

Forgetting works the same on all kinds. The mode (soft/hard) is the user's choice; the kind doesn't affect the operation.

## 7. Filters by kind

`RECALL` accepts an optional `kind_filter`:

- `None` — all kinds (default).
- `Some([Episodic])` — only episodic.
- `Some([Semantic, Consolidated])` — exclude episodic.
- etc.

The set form lets the agent narrow to the kinds it expects (e.g., "tell me what's stable about the user" → semantic + consolidated).

## 8. Kind defaults summary

| Kind | Initial salience weight | Decay half-life | Eviction priority | Created by |
|---|---|---|---|---|
| Episodic | 0.5 | 30 days | Highest (first) | Agent (default) |
| Semantic | 0.7 | 365 days | Lowest (last) | Agent (explicit), promotion |
| Consolidated | 0.6 | 90 days | Middle | Consolidation worker only |

These constants are tunable. The relative ordering (episodic decays fastest, semantic decays slowest, consolidated in between) is the design choice; the specific numbers are calibrated against observed agent behavior on benchmark workloads.

## 9. Why three kinds, not more

Brain considered:

- **One kind.** Lose the distinction; everything is treated equally. Salience alone wouldn't capture the differences in cognitive role.
- **Two kinds (episodic, semantic).** Closer to classical cognitive science. Loses the explicit category for "things the substrate noticed", which is what `Consolidated` represents.
- **Many kinds** (working, autobiographical, procedural, declarative, etc.). Most of the additional categories don't have clear operational distinctions in our system. Procedural memory is an interesting candidate (deferred to v2; see [01.10 OQ-8](../01_system_architecture/10_open_questions.md)).

The chosen three are well-grounded: two from classical theory plus one operational category for substrate-derived patterns. They map onto distinct decay profiles, distinct salience baselines, and distinct creation paths.

## 10. Wire and storage representation

| Context | Representation |
|---|---|
| Wire (rkyv-encoded) | u8 (0=Episodic, 1=Semantic, 2=Consolidated) |
| Storage (redb) | u8 |
| Memory record (in-memory) | enum `MemoryKind` |
| SDK (typed) | language-native enum |

The on-the-wire and on-disk values are stable. New kinds (in v2 or later) would be added with new numeric values; old readers seeing an unknown kind reject it.

---

*Continue to [`08_lifecycle.md`](08_lifecycle.md) for memory lifecycle.*
