# 02.11 Open Questions

Data-model-level questions unresolved as of this spec version. These are narrower than the architectural open questions in [01.10](../01_system_architecture/10_open_questions.md); they concern the entities and their relationships specifically.

---

## DM-OQ-1: Multi-context membership

**Issue.** A memory belongs to exactly one context. Some applications would benefit from memories belonging to multiple contexts simultaneously (a project insight that's also relevant to "lessons learned").

**Options.**

a) **Stay single-context.** Multi-context memories are encoded twice. Storage cost is doubled; consistency is the application's problem.

b) **Allow multi-context.** Each memory carries a list of context IDs. Storage cost grows with context-count overlap. Filter evaluation is slightly more expensive but tractable.

c) **Add tags as a separate concept.** Tags are lightweight, multi-attach, used for soft filtering. Contexts remain single-attach for primary scope.

**Recommendation.** Defer. If user feedback indicates contexts-as-tags would significantly help, revisit with option (c) — tags as a separate concept rather than expanding contexts.

---

## DM-OQ-2: Soft contexts (boolean expressions over contexts)

**Issue.** `RECALL` accepts a context filter as a single id or a small set. Some applications want richer expressions: "in context A, but exclude memories also in context B" or "in any of contexts {A, B, C} but not D".

**Options.**

a) **Allow boolean expressions.** Define a small filter language. Adds complexity to the planner and to wire encoding.

b) **Stay with set membership.** Applications express complex filters at the application layer (multiple queries, post-filtering).

**Recommendation.** Stay with set membership in v1. Revisit if real workloads consistently need boolean expressions.

---

## DM-OQ-3: Memory provenance richness

**Issue.** A memory carries `source_request_id` (which encode created it) and `embedding_model_fp` (which model embedded it). These are minimal provenance. Some applications want richer trails: tool calls that produced the text, prior memory ids that informed the encode, etc.

**Options.**

a) **Add a generic `provenance: Map<String, Bytes>` field** allowing arbitrary key-value annotations.

b) **Add specific fields** like `produced_by_tool: Option<String>`, `informed_by: Vec<MemoryId>` — typed but not extensible.

c) **Use edges.** A memory's provenance is captured by `DERIVED_FROM` and `REFERENCES` edges; agents express provenance through edges.

**Recommendation.** Use option (c). Edges are the right abstraction for "what informed this memory". Adding provenance fields multiplies schema; using edges keeps the model uniform.

---

## DM-OQ-4: Vector storage precision

**Issue.** Vectors are stored as `f32`. Some workloads could tolerate `f16` (half precision; 2 bytes per element instead of 4) for 2× density at modest recall cost. `i8` quantization gives 4× density at larger recall cost.

**Options.**

a) **Single precision (f32) only.** Simplest; current spec.

b) **Optional `f16`.** Per-shard configuration; quantized arenas use 2 bytes per element.

c) **Optional `i8`.** Per-shard; 4× density.

d) **Product quantization (PQ).** More complex; better compression at higher cost.

**Recommendation.** Defer. Revisit after benchmarks reveal whether storage density is actually a problem for typical workloads.

---

## DM-OQ-5: Composite memories

**Issue.** A memory has one text and one vector. What if the agent wants to encode a structured observation: "user said X via channel Y at time Z while in state S"?

**Options.**

a) **Application encodes structured data as text.** Text is just JSON or natural language; agent parses it back.

b) **Add structured metadata fields.** A few well-defined fields beyond text: channel, modality, etc.

c) **Composite memories.** A memory has multiple "facets" — different texts, different vectors, different views.

**Recommendation.** Stay with single text + vector. Structured data is the application's problem; the substrate operates on text.

---

## DM-OQ-6: Agent inheritance / hierarchy

**Issue.** Some applications would benefit from agents inheriting context: a "team agent" with shared memory, plus per-user sub-agents with private memory plus access to the team's.

**Options.**

a) **Out of scope.** Each agent's memory is fully isolated; sharing is the application's problem.

b) **Read-only links.** An agent can subscribe to read-only access to another agent's memories under specific contexts.

c) **Hierarchical agents.** First-class parent-child relationships.

**Recommendation.** Out of scope; this is application-layer composition. If a clear cross-agent sharing pattern emerges, revisit.

---

## DM-OQ-7: Soft delete recovery window

**Issue.** Soft-forgotten memories are recoverable until the slot is reclaimed. There's no explicit recovery window, no "undo" within X seconds. Should there be?

**Options.**

a) **Stay implicit.** Recovery is best-effort, depends on slot pressure.

b) **Add an explicit window.** Forgotten memories are guaranteed recoverable for N minutes; only after that does the slot become eligible for reuse.

c) **Add an explicit `UNDO_FORGET` operation** that restores a recently-forgotten memory.

**Recommendation.** Defer. Most agents don't need undo; the rare cases can be handled with snapshots.

---

## DM-OQ-8: Edge weight calibration

**Issue.** Edge weights are in [0, 1] but the substrate doesn't calibrate them. A `CAUSED` edge with weight 0.7 from one agent and 0.7 from another mean different things if the agents' calibrations differ.

**Options.**

a) **Stay uncalibrated.** Weights are agent-specific; cross-agent comparison is the application's problem.

b) **Calibrate weights via observation.** If the substrate sees that "agent A's 0.7 typically corresponds to outcomes similar to agent B's 0.5", apply a per-agent transformation.

**Recommendation.** Stay uncalibrated. Calibration would require ground-truth signal we don't have.

---

## DM-OQ-9: Memory text and vector coupling

**Issue.** A memory's vector is the embedding of its text. They're coupled. If text is updated (a typo correction), the vector should be re-computed.

**Options.**

a) **No text update.** Memories are immutable except for forgetting. Corrections are new memories with `REFERENCES` edges to the original.

b) **Text update with re-embed.** A new operation `UPDATE_TEXT` re-embeds and updates.

c) **Text update without re-embed.** Update text but leave vector. Corrupts the coupling invariant.

**Recommendation.** Option (a). Memories are immutable. Corrections are explicitly new memories. This is consistent with the immutability principle and avoids the temptation to mutate stored history.

---

*Continue to [`12_references.md`](12_references.md) for further reading.*
