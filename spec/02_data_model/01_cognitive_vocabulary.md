# 02.01 Cognitive Vocabulary

The words we use shape what's easy to think and what's hard. Brain chose its vocabulary deliberately. This file documents the choices and the alternatives rejected.

The vocabulary is summarized in [01.09 Glossary](../01_system_architecture/09_glossary.md) and mirrored in [00.01 Glossary](../00_master_overview/01_glossary.md). This document explains *why* the chosen terms — not what they mean.

## 1. The central word: memory

We call the unit of storage a **memory**. We rejected:

- **Document** — too web-search-y. A document is text-shaped. A memory might be a fact, an event, or an inference; "document" overconstrains it.
- **Item** — too generic. Doesn't carry the right connotation of "something the agent remembers".
- **Entry** — too log-flavored. Implies append-only sequence; ours has structure and lifecycle.
- **Vector** — internally accurate (the central piece is a vector), but too implementation-focused. Clients shouldn't think about vectors; they should think about memories.
- **Note** — close, but suggests deliberate human-style note-taking. Memories include automatic captures and consolidations, not just notes.
- **Embedding** — implementation detail leaking into vocabulary.

**Memory** is the right level of abstraction. It connotes:

- The thing is *remembered* — has time, has decay, can be forgotten.
- The thing has *content* — text, structure.
- The thing is *recallable* — by similarity, by reference, by association.
- The thing is *agent-owned* — agents have memories; memories belong to agents.

We're using the word in its everyday sense, not in any specific cognitive-science technical sense. There's no intent to claim biological correspondence; the word is chosen for its connotations.

## 2. The verbs: encode, recall, plan, reason, forget

The five primitives use cognitive-science verbs:

- **Encode** rather than `write`, `insert`, `put`, `create`, `store`. *Encode* implies transformation — the input text is transformed into a stored representation. *Write* implies a passive deposit; *insert* implies position in a sequence. Brain does neither cleanly: it transforms, and the transformation is the point.

- **Recall** rather than `read`, `get`, `query`, `search`, `find`. *Recall* implies similarity-based retrieval, possibly imperfect, ranked by relevance. *Read* implies exact lookup. *Query* implies a database; *search* implies a corpus. Brain's read primitive is closer to remembering than to looking up.

- **Plan** rather than `traverse`, `path`, `route`, `solve`. *Plan* is the verb cognitive scientists use for constructing action sequences toward goals. The other verbs are correct in some literal sense but miss the goal-oriented framing.

- **Reason** rather than `infer`, `derive`, `explain`, `deduce`. *Reason* is broader than *infer* (which often means pure logical deduction); narrower than *explain* (which can be after-the-fact). The right word for "given an observation, work out what stored memories make sense of it".

- **Forget** rather than `delete`, `remove`, `drop`, `purge`. *Forget* admits both deliberate and gradual removal — Brain has both. *Delete* implies hard removal only; *remove* is slightly more general but doesn't connote the cognitive sense.

## 3. The notions: salience, decay, consolidation

These three are direct lifts from cognitive science / neuroscience.

### Salience

A score representing "how important is this memory?". We rejected:

- **Importance** — an OK English word but too vague.
- **Score** — generic; doesn't say what the score measures.
- **Weight** — implies use in a weighted-average computation; too narrow.
- **Priority** — implies queueing; doesn't carry "this matters because it's relevant or surprising".

**Salience** is the cognitive-science term and it's what we mean. Salience reflects relevance, recency, surprise, and explicit importance. It's a single number in [0, 1] but emerges from multiple inputs.

### Decay

The exponential lowering of salience over time. The term "decay" is direct from the [Ebbinghaus forgetting curve](https://en.wikipedia.org/wiki/Forgetting_curve). We rejected:

- **Aging** — too neutral; doesn't connote loss.
- **Expiry / TTL** — implies a hard cutoff; we don't have one.
- **Degradation** — implies content corruption; salience decay doesn't change content.

### Consolidation

The background process that summarizes related episodic memories into semantic ones. The term comes from [memory consolidation](https://en.wikipedia.org/wiki/Memory_consolidation) in neuroscience — the process by which experiences are integrated into long-term memory, often during sleep.

This term is the closest fit. Alternatives:

- **Compression** — too storage-flavored; suggests no semantic change.
- **Summarization** — close, but consolidation includes more than summarization (clustering, pattern extraction).
- **Aggregation** — too data-warehouse-y.

## 4. The structural words: edge, kind, context

### Edge

A typed link between two memories. Standard graph-theory term. Alternatives:

- **Link** — overloaded with hyperlinks.
- **Relation** — sounds relational-database.
- **Connection** — too vague; could be socket-flavored.

**Edge** is the right word. It comes with the implicit baggage of graph theory (directed, may have weights, traversed in operations) which is what we want.

### Kind

The classification of a memory: episodic, semantic, consolidated. We rejected:

- **Type** — overloaded in programming contexts.
- **Class** — programming overload + sociological overload.
- **Category** — generic; doesn't carry the trichotomy we mean.

**Kind** is intentionally simple and unique to this vocabulary. Three kinds, fixed enum, easy to reason about.

### Context

A logical scope within an agent's memory. We rejected:

- **Namespace** — too database-flavored.
- **Topic** — too topical; contexts can be cross-topic.
- **Tag** — multi-attach; we want each memory in exactly one context (in v1).
- **Project** — too application-specific.
- **Folder** — too filesystem-flavored.
- **Collection** — generic.

**Context** matches what cognitive scientists mean by "the situational context of a memory". It's the right level of abstraction for "this memory is from work, that one is from home".

## 5. The metadata words

### Confidence

A normalized score in [0, 1] for `RECALL` results. *Confidence* is the right ML term — it implies a calibrated probability, not just a similarity score. We provide actual calibration ([01.06 Targets](../01_system_architecture/06_targets.md) §4.2) so the word fits.

Alternatives rejected:

- **Score** — uncalibrated.
- **Similarity** — implementation-flavored, and we expose more than raw similarity.
- **Probability** — too narrow; confidence reflects more than raw probability.

### Tombstone

The lifecycle state of a forgotten-but-not-yet-reclaimed slot. Standard distributed-systems vocabulary. Alternatives:

- **Deleted** — too final; tombstones are recoverable.
- **Hidden** — too mild; tombstones are excluded from queries.
- **Soft-deleted** — accurate but verbose.

**Tombstone** is the database term and we use it.

### Reclaimed

The lifecycle state after a tombstoned slot has been reused. Standard memory-management term.

## 6. The implementation-flavored words we hide

Some words exist in the implementation but don't surface in the public vocabulary:

- **Slot** — the fixed-size cell in the arena. Internal; clients don't see slots.
- **Arena** — the mmap'd file. Internal; clients don't know it exists.
- **WAL** — the write-ahead log. Internal; clients see no logs.
- **HNSW** — the ANN algorithm. Internal; clients don't choose it.
- **Vector** — the 384-dim float array. Internal; clients send text.
- **Epoch** — the lock-free reclamation marker. Internal; clients don't know about epochs.

This isn't accidental. The substrate is opinionated about hiding implementation. A client SDK that exposed `slot_id` directly would let users build code that depends on internal stability we don't promise. By keeping these words inside, we keep our right to evolve them.

## 7. The agent-application words we don't use

Words from the application layer that *don't* belong in our vocabulary:

- **Prompt** — what the agent feeds the LLM. Brain has nothing to do with prompts.
- **Conversation** — a sequence of agent-user exchanges. May be encoded as memories, but the substrate doesn't model conversations.
- **Tool / Action** — agent-side concepts. Outside Brain's scope.
- **Session** — overloaded. We use *session* only for the connection-level concept ([01.09 Glossary](../01_system_architecture/09_glossary.md)).
- **User** — the human or system the agent serves. Brain doesn't model users; agents do.

These are intentionally absent from our vocabulary. The substrate provides cognitive operations; how an application maps prompts/conversations/tools/users onto the substrate is the application's design choice.

## 8. Summary

The vocabulary in one sentence: an **agent** has **memories** with **salience** that **decay** over time, organized by **context** and connected by typed **edges** of various **kinds**, and the agent uses cognitive primitives — **encode**, **recall**, **plan**, **reason**, **forget** — to interact with them.

If a future spec uses different words for the same concepts, this vocabulary wins; the offending spec is wrong.

---

*Continue to [`02_memory_entity.md`](02_memory_entity.md) for the central entity.*
